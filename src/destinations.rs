use super::{Dictionary, Document, Object, PdfName, Result};
use indexmap::IndexMap;
#[derive(Debug, Clone)]
pub struct Destination(Dictionary);

impl Destination {
    pub fn new(title: Object, page: Object, typ: Object) -> Self {
        let mut dict = Dictionary::new();
        dict.set(b"Title", title);
        dict.set(b"Page", page);
        dict.set(b"Type", typ);
        Destination(dict)
    }

    pub fn set<K, V>(&mut self, key: K, value: V)
    where
        K: Into<PdfName>,
        V: Into<Object>,
    {
        self.0.set(key, value);
    }

    pub fn title(&self) -> Result<&Object> {
        self.0.get(b"Title")
    }

    pub fn page(&self) -> Result<&Object> {
        self.0.get(b"Page")
    }

    /// Any additional outline attribute copied from the item dictionary
    /// (`/Count`, `/F`, `/C`, `/Action`).
    pub fn get(&self, key: &[u8]) -> Result<&Object> {
        self.0.get(key)
    }
}

impl Document {
    pub fn get_named_destinations(
        &self,
        tree: &Dictionary,
        named_destinations: &mut IndexMap<Vec<u8>, Destination>,
    ) -> Result<()> {
        self.get_named_destinations_impl(tree, named_destinations, 0)
    }

    fn get_named_destinations_impl(
        &self,
        tree: &Dictionary,
        named_destinations: &mut IndexMap<Vec<u8>, Destination>,
        depth: usize,
    ) -> Result<()> {
        if depth >= crate::reader::MAX_NESTING_DEPTH {
            return Err(crate::Error::RecursionLimit);
        }
        if let Ok(kids) = tree.get(b"Kids") {
            for kid in kids.as_array()? {
                if let Ok(kid) = kid
                    .as_reference()
                    .and_then(move |id| self.get_dictionary(id))
                {
                    self.get_named_destinations_impl(kid, named_destinations, depth + 1)?;
                }
            }
        }
        // /Names may be an indirect reference (writers commonly store the
        // array out-of-line); resolve before reading (pdfium
        // CPDF_NameTree::Lookup old-walk dereferences too).
        if let Ok(names) = tree
            .get(b"Names")
            .and_then(|object| self.dereference(object))
            .map(|(_, object)| object.clone())
        {
            let mut names = names.as_array()?.iter();
            loop {
                let key = names.next();
                if key.is_none() {
                    break;
                }
                let val = names.next();
                if val.is_none() {
                    break;
                }
                // A name-tree key must be a string; the value is either a
                // reference to an action dict (/D destination), a direct
                // destination array, or the destination dict itself.
                // Malformed entries skip without panicking.
                let Some(key) = key.and_then(|object| object.as_str().ok().map(|key| key.to_vec())) else {
                    continue;
                };
                let value = match val.and_then(|object| object.as_reference().ok()) {
                    Some(obj_ref) => match self.get_object(obj_ref) {
                        Ok(Object::Dictionary(dict)) => dict
                            .get(b"D")
                            .and_then(Object::as_array)
                            .ok()
                            .map(|array| array.clone()),
                        Ok(Object::Array(array)) => Some(array.clone()),
                        _ => None,
                    },
                    _ => match val.and_then(|object| object.as_array().ok()) {
                        Some(array) => Some(array.clone()),
                        None => match val.and_then(|object| object.as_dict().ok()) {
                            Some(dict) => dict.get(b"D").and_then(Object::as_array).ok().map(|array| array.clone()),
                            None => None,
                        },
                    },
                };
                if let Some(array) = value {
                    if array.len() >= 2 {
                        let dest =
                            Destination::new(Object::string_literal(key.clone()), array[0].clone(), array[1].clone());
                        named_destinations.insert(key, dest);
                    }
                }
            }
        }
        Ok(())
    }
}

