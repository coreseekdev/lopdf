use indexmap::IndexMap;

use super::{Destination, Dictionary, Document, Error, Object, ObjectId, Result};

#[derive(Debug, Clone)]
pub enum Outline {
    Destination(Destination),
    SubOutlines(Vec<Outline>),
}

/// Copies outline presentation attributes from the item dictionary to the
/// resolved destination: `/Count` (open/closed), `/F` (style bits) and `/C`
/// (colour).  The content layer surfaces them as-is.
fn copy_outline_attributes(node: &Dictionary, destination: &mut Destination) {
    for key in [b"Count".as_slice(), b"F", b"C"] {
        if let Ok(value) = node.get(key) {
            destination.set(key, value.clone());
        }
    }
}

impl Document {
    pub fn get_outline(
        &self,
        node: &Dictionary,
        named_destinations: &mut IndexMap<Vec<u8>, Destination>,
    ) -> Result<Option<Outline>> {
        let action = match self.get_dict_in_dict(node, b"A") {
            Ok(a) => a,
            Err(_) => {
                return self.build_outline_result(
                    node.get(b"Dest")?,
                    node.get(b"Title")?,
                    Some(node),
                    named_destinations,
                );
            }
        };
        let command = action.get(b"S")?.as_name()?;
        if command != b"GoTo" && command != b"GoToR" {
            // A non-GoTo action (URI, Launch, JavaScript, Named, ...) is
            // still a bookmark: list it with its action instead of dropping
            // the item.  `/Dest` is absent, so the page stays unresolved.
            let title = node.get(b"Title")?;
            let mut destination = Destination::new(title.clone(), Object::Null, Object::Null);
            destination.set(b"Action", Object::Name(command.to_vec().into()));
            copy_outline_attributes(node, &mut destination);
            return Ok(Some(Outline::Destination(destination)));
        }
        let title_obj = node.get(b"Title")?;
        let title_ref = match title_obj.as_reference() {
            Ok(o) => o,
            Err(_) => match title_obj.as_str() {
                Ok(_) => {
                    return self.build_outline_result(
                        action.get(b"D")?,
                        title_obj,
                        Some(node),
                        named_destinations,
                    );
                }
                Err(err) => return Err(err),
            },
        };
        self.build_outline_result(
            action.get(b"D")?,
            self.get_object(title_ref)?,
            Some(node),
            named_destinations,
        )
    }

    pub fn get_outlines(
        &self,
        node: Option<Object>,
        outlines: Option<Vec<Outline>>,
        named_destinations: &mut IndexMap<Vec<u8>, Destination>,
    ) -> Result<Option<Vec<Outline>>> {
        self.get_outlines_impl(node, outlines, named_destinations, 0)
    }

    fn get_outlines_impl(
        &self,
        mut node: Option<Object>,
        mut outlines: Option<Vec<Outline>>,
        named_destinations: &mut IndexMap<Vec<u8>, Destination>,
        depth: usize,
    ) -> Result<Option<Vec<Outline>>> {
        if depth >= crate::reader::MAX_NESTING_DEPTH {
            return Err(Error::RecursionLimit);
        }
        // A damaged file can make the /Next sibling chain cyclic.  Track the
        // object identities we have followed; pdfium cpdf_bookmarktree.cpp
        // and pdf.js catalog.js both stop on a repeated edge.
        let mut visited = std::collections::BTreeSet::new();
        if let Some(Object::Reference(id)) = node.as_ref() {
            visited.insert(*id);
        }
        if outlines.is_none() {
            outlines = Some(Vec::new());
            let catalog = self.catalog()?;
            let mut dict_node = self.get_dict_in_dict(catalog, b"Outlines")?;
            if let Ok(Object::Reference(id)) = dict_node.get(b"First") {
                visited.insert(*id);
            }
            let first = self.get_dict_in_dict(dict_node, b"First");
            if let Ok(first) = first {
                dict_node = first;
            }
            // /Names /Dests is authoritative; the catalog /Dests
            // dictionary is the legacy fallback (pdfium
            // cpdf_nametree.cpp:592-606; pdf.js catalog.js:92-102).
            let mut tree = self
                .get_dict_in_dict(catalog, b"Names")
                .ok()
                .and_then(|names| self.get_dict_in_dict(names, b"Dests").ok())
                .ok_or(Error::ObjectNotFound((0, 0)));
            if tree.is_err() {
                tree = self.get_dict_in_dict(catalog, b"Dests");
            }
            if let Ok(tree) = tree {
                self.get_named_destinations(tree, named_destinations)?;
            }
            node = Some(Object::Dictionary(dict_node.clone()));
        }
        if node.is_none() {
            return Ok(outlines);
        }
        let node = node.unwrap();
        let mut node = match node.as_dict() {
            Ok(n) => n.clone(),
            Err(_) => self.get_object(node.as_reference()?)?.as_dict()?.clone(),
        };
        loop {
            if let Ok(Some(outline)) = self.get_outline(&node, named_destinations)
                && let Some(ref mut outlines) = outlines
            {
                outlines.push(outline);
            }
            if let Ok(first) = node.get(b"First") {
                let sub_outlines = Vec::new();
                let sub_outlines = self.get_outlines_impl(
                    Some(first.clone()),
                    Some(sub_outlines),
                    named_destinations,
                    depth + 1,
                )?;
                if let Some(sub_outlines) = sub_outlines
                    && !sub_outlines.is_empty()
                    && let Some(ref mut outlines) = outlines
                {
                    outlines.push(Outline::SubOutlines(sub_outlines));
                }
            }
            let next = match node.get(b"Next") {
                Ok(next) => next.clone(),
                Err(_) => break,
            };
            if let Object::Reference(id) = &next
                && !visited.insert(*id)
            {
                break;
            }
            node = match next {
                Object::Dictionary(dict) => dict,
                Object::Reference(id) => match self.get_object(id) {
                    Ok(Object::Dictionary(dict)) => dict.clone(),
                    _ => break,
                },
                _ => break,
            };
        }
        Ok(outlines)
    }

    pub fn delete_outlines(&mut self) -> Result<()> {
        if let Some(outlines_id) = self
            .catalog()?
            .get(b"Outlines")
            .ok()
            .and_then(|v| v.as_reference().ok())
        {
            let outlines = self.get_dictionary(outlines_id)?;
            if let Ok(Object::Reference(first)) = outlines.get(b"First") {
                self.walk_outlines_del(*first, 0)?;
            }
            self.delete_object(outlines_id);
        }

        Ok(())
    }

    fn build_outline_result(
        &self,
        dest: &Object,
        title: &Object,
        node: Option<&Dictionary>,
        named_destinations: &mut IndexMap<Vec<u8>, Destination>,
    ) -> Result<Option<Outline>> {
        let outline = match dest {
            Object::Array(obj_array) => {
                let mut destination = Destination::new(
                    title.to_owned(),
                    obj_array[0].clone(),
                    obj_array.get(1).cloned().unwrap_or(Object::Null),
                );
                if let Some(node) = node {
                    copy_outline_attributes(node, &mut destination);
                }
                Outline::Destination(destination)
            }
            Object::String(key, _) => {
                if let Some(destination) = named_destinations.get(key.as_slice()) {
                    let mut destination = destination.clone();
                    destination.set(b"Title", title.to_owned());
                    if let Some(node) = node {
                        copy_outline_attributes(node, &mut destination);
                    }
                    Outline::Destination(destination)
                } else {
                    return Ok(None);
                }
            }
            Object::Name(key) => {
                if let Some(destination) = named_destinations.get(key.as_slice()) {
                    let mut destination = destination.clone();
                    destination.set(b"Title", title.to_owned());
                    if let Some(node) = node {
                        copy_outline_attributes(node, &mut destination);
                    }
                    Outline::Destination(destination)
                } else {
                    return Ok(None);
                }
            }
            Object::Reference(object_id) => {
                return self.build_outline_result(
                    self.get_object(*object_id)?,
                    title,
                    node,
                    named_destinations,
                );
            }
            _ => {
                return Err(Error::InvalidOutline(format!(
                    "Unexpected destination {dest:?}"
                )));
            }
        };
        Ok(Some(outline))
    }

    fn walk_outlines_del(&mut self, outline: ObjectId, depth: usize) -> Result<()> {
        if depth >= crate::reader::MAX_NESTING_DEPTH {
            return Err(Error::RecursionLimit);
        }
        let mut cur = Some(outline);

        while let Some(id) = cur.take() {
            let outline = self.get_dictionary(id)?;
            let first = outline
                .get(b"First")
                .ok()
                .and_then(|v| v.as_reference().ok());
            let next = outline
                .get(b"Next")
                .ok()
                .and_then(|v| v.as_reference().ok());

            if let Some(first) = first {
                self.walk_outlines_del(first, depth + 1)?;
            }
            cur = next;
            self.delete_object(id);
        }

        Ok(())
    }
}
