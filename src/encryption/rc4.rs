// This module exists because the rust-crypto module is really old and not maintained.
// Fortunately the RC4 algorithm is very simple to implement.
pub struct Rc4 {
    initial_state: [u8; 256],
}

impl Rc4 {
    pub(crate) fn initial_state(&self) -> [u8; 256] {
        self.initial_state
    }

    pub fn new<Key: AsRef<[u8]>>(key: Key) -> Self {
        let key = key.as_ref();
        assert!(!key.is_empty() && key.len() <= 256);

        // PDF encryption produces per-object keys of fixed, short lengths
        // (5, 10 or 16 bytes).  Specialising the KSA on those lengths lets the
        // compiler unroll the key repetition away; the generic path still
        // handles everything else.
        let initial_state = match key.len() {
            5 => ksa_array::<5>(key.try_into().expect("length checked")),
            10 => ksa_array::<10>(key.try_into().expect("length checked")),
            16 => ksa_array::<16>(key.try_into().expect("length checked")),
            32 => ksa_array::<32>(key.try_into().expect("length checked")),
            _ => ksa_generic(key),
        };

        Self { initial_state }
    }

    /// Encrypt/decrypt with a caller-owned state.  The state is consumed, so
    /// callers that keep a pristine copy can pass a scratch copy instead of
    /// letting [`decrypt`] duplicate it.
    pub(crate) fn decrypt_state(state: &mut [u8; 256], input: &[u8]) -> Vec<u8> {
        let mut output = vec![0; input.len()];
        Self::apply_keystream_state(state, input.iter(), output.iter_mut());
        output
    }

    pub(crate) fn encrypt_state(state: &mut [u8; 256], input: &[u8]) -> Vec<u8> {
        Self::decrypt_state(state, input)
    }

    fn apply_keystream_state<'i, 'o, Input, Output>(
        state: &mut [u8; 256],
        input: Input,
        output: Output,
    ) where
        Input: Iterator<Item = &'i u8>,
        Output: Iterator<Item = &'o mut u8>,
    {
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

/// RC4 identity permutation.  A constant table initialises the state with
/// wide stores instead of 256 closure calls.
const IDENTITY: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut i = 0usize;
    while i < 256 {
        table[i] = i as u8;
        i += 1;
    }
    table
};

/// Key-schedule a key of compile-time length.  Each pass over the key is
/// fully unrolled, so no repeated-key buffer or modulo is needed in the hot
/// loop; the short tail (256 % N steps) uses the modulo form.
fn ksa_array<const N: usize>(key: &[u8; N]) -> [u8; 256] {
    let mut state = IDENTITY;
    let mut j = 0_u8;
    let mut i = 0usize;
    while i + N <= 256 {
        let mut k = 0usize;
        while k < N {
            j = j.wrapping_add(state[i + k]).wrapping_add(key[k]);
            state.swap(i + k, j as usize);
            k += 1;
        }
        i += N;
    }
    while i < 256 {
        j = j.wrapping_add(state[i]).wrapping_add(key[i % N]);
        state.swap(i, j as usize);
        i += 1;
    }
    state
}

fn ksa_generic(key: &[u8]) -> [u8; 256] {
    let mut state = IDENTITY;
    let mut j = 0_u8;
    let mut i = 0usize;
    while i < 256 {
        j = j
            .wrapping_add(state[i])
            .wrapping_add(key[i % key.len()]);
        state.swap(i, j as usize);
        i += 1;
    }
    state
}
