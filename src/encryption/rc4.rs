// This module exists because the rust-crypto module is really old and not maintained.
// Fortunately the RC4 algorithm is very simple to implement.
pub struct Rc4 {
    initial_state: [u8; 256],
}

impl Rc4 {
    pub(crate) fn initial_state(&self) -> [u8; 256] {
        self.initial_state
    }

    pub(crate) fn from_state(initial_state: [u8; 256]) -> Self {
        Self { initial_state }
    }

    pub fn new<Key: AsRef<[u8]>>(key: Key) -> Self {
        let key = key.as_ref();
        assert!(!key.is_empty() && key.len() <= 256);

        let mut initial_state = std::array::from_fn(|i| i as u8);

        // Materialise the repeated key once; the KSA then has no modulo or
        // wrap branch in its 256-iteration hot loop.
        let mut repeated_key = [0u8; 256];
        let mut offset = 0;
        while offset < 256 {
            let take = key.len().min(256 - offset);
            repeated_key[offset..offset + take].copy_from_slice(&key[..take]);
            offset += take;
        }
        let mut j = 0_u8;
        macro_rules! ksa_step {
            ($index:expr) => {{
                j = j
                    .wrapping_add(initial_state[$index])
                    .wrapping_add(repeated_key[$index]);
                initial_state.swap($index, j as usize);
            }};
        }
        let mut i = 0;
        while i < 256 {
            ksa_step!(i);
            ksa_step!(i + 1);
            ksa_step!(i + 2);
            ksa_step!(i + 3);
            i += 4;
        }

        Self { initial_state }
    }

    /// Encrypts/decrypts `input` into `output`.  The shorter of `input` and `output`
    ///  determine how many bytes are written into `output`.
    pub fn apply_keystream<'i, 'o, Input, Output>(&self, input: Input, output: Output)
    where
        Input: Iterator<Item = &'i u8>,
        Output: Iterator<Item = &'o mut u8>,
    {
        let mut state = self.initial_state;
        let mut i = 0_u8;
        let mut j = 0_u8;
        for (i_byte, o_byte) in input.zip(output) {
            i = i.wrapping_add(1);
            j = j.wrapping_add(state[i as usize]);
            state.swap(i as usize, j as usize);
            let key_byte = state[(state[i as usize].wrapping_add(state[j as usize])) as usize];
            *o_byte = i_byte ^ key_byte;
        }
    }

    /// Allocates a new Vec<u8> of the same length as `input` and decrypts
    ///  `input` into it.
    pub fn decrypt<Input>(&self, input: Input) -> Vec<u8>
    where
        Input: AsRef<[u8]>,
    {
        let input = input.as_ref();
        let mut output = vec![0; input.len()];
        self.apply_keystream(input.iter(), output.iter_mut());
        output
    }

    /// Allocates a new Vec<u8> of the same length as `input` and encrypts
    ///  `input` into it.
    pub fn encrypt<Input>(&self, input: Input) -> Vec<u8>
    where
        Input: AsRef<[u8]>,
    {
        // Rc4 is symmetric
        self.decrypt(input)
    }
}
