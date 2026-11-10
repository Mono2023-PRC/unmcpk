//! Port of the RTR ("rotor") cipher used to wrap MCPK file bodies.
//!
//! The machine is a chain of `n` 256-byte substitution tables (rotors) whose
//! positions advance per byte, seeded by a key-dependent LCG.  Everything is
//! byte oriented, so the whole thing is a pure function of (key, data) — state
//! is reset on every `d_region` call, which makes decoding parallelisable.

/// Seed fragment the obfuscator has been using since at least the 2020 builds.
pub const SEED: &[u8] = b"j2h56ogodh3sk";

/// Expanded rotor key, matching the reference implementation byte for byte:
/// `key = seed*4 + (a+seed+b)*5 + "$@" + a*7 + b*2 + "&]`"` with
/// a = "=dziaq;", b = "|`o=5v7!\"-276".
pub fn default_key() -> Vec<u8> {
    const A: &[u8] = b"=dziaq;";
    const B: &[u8] = b"|`o=5v7!\"-276";
    let mut key = Vec::with_capacity(SEED.len() * 4 + (A.len() + SEED.len() + B.len()) * 5 + 40);
    for _ in 0..4 {
        key.extend_from_slice(SEED);
    }
    for _ in 0..5 {
        key.extend_from_slice(A);
        key.extend_from_slice(SEED);
        key.extend_from_slice(B);
    }
    key.extend_from_slice(b"$@");
    for _ in 0..7 {
        key.extend_from_slice(A);
    }
    for _ in 0..2 {
        key.extend_from_slice(B);
    }
    key.extend_from_slice(b"&]`");
    key
}

/// `(k << 3) | (k >> 13)` — the 16-bit rotate step of the key schedule.
#[inline]
fn rot(k: u64) -> u64 {
    (k << 3) | (k >> 13)
}

/// A rotor machine instance. Cheap to build; callers keep one per thread.
#[derive(Clone)]
pub struct Rotor {
    key: [i16; 5],
    seed: [i32; 3],
    size: usize,
    size_mask: usize,
    rotors: usize,
    e_rotor: Vec<u8>,
    d_rotor: Vec<u8>,
    positions: Vec<u8>,
    advances: Vec<u8>,
    inited: bool,
}

impl Rotor {
    pub fn new(rotors: usize, key: &[u8]) -> Self {
        let mut r = Rotor {
            key: [0; 5],
            seed: [0; 3],
            size: 256,
            size_mask: 255,
            rotors,
            e_rotor: vec![0; rotors * 256],
            d_rotor: vec![0; rotors * 256],
            positions: vec![0; rotors],
            advances: vec![0; rotors],
            inited: false,
        };
        r.set_key(key);
        r
    }

    #[inline]
    fn set_key(&mut self, key: &[u8]) {
        let mut k1: u64 = 995;
        let mut k2: u64 = 576;
        let mut k3: u64 = 767;
        let mut k4: u64 = 671;
        let mut k5: u64 = 463;
        // Go gives `|` and `+`/`-`/`^` the same precedence and evaluates left
        // to right, so `(k << 3) | (k >> 13) + ki` means `(rotate(k)) + ki`,
        // not `(k << 3) | ((k >> 13) + ki)`. Getting this wrong silently
        // produces a key that decodes nothing.
        for &b in key {
            let ki = b as u64;
            k1 = (rot(k1).wrapping_add(ki)) & 0xffff;
            k2 = (rot(k2) ^ ki) & 0xffff;
            k3 = rot(k3).wrapping_sub(ki) & 0xffff;
            k4 = ki.wrapping_sub(rot(k4)) & 0xffff;
            k5 = (rot(k5) ^ ((!ki) & 0xffff)) & 0xffff;
        }
        self.key = [k1 as i16, (k2 | 1) as i16, k3 as i16, k4 as i16, k5 as i16];
        self.seed = [self.key[0] as i32, self.key[1] as i32, self.key[2] as i32];
        self.inited = false;
    }

    /// LCG in [0, 1) - the classic Wichmann-Hill generator.
    fn r_random(&mut self) -> f64 {
        let (mut x, mut y, mut z) = (self.seed[0], self.seed[1], self.seed[2]);
        x = 171 * (x % 177) - 2 * (x / 177);
        y = 172 * (y % 176) - 35 * (y / 176);
        z = 170 * (z % 178) - 63 * (z / 178);
        if x < 0 {
            x += 30269;
        }
        if y < 0 {
            y += 30307;
        }
        if z < 0 {
            z += 30323;
        }
        self.seed = [x, y, z];
        let term = x as f64 / 30269.0 + y as f64 / 30307.0 + z as f64 / 30323.0;
        let mut val = term - term.floor();
        if val >= 1.0 {
            val = 0.0;
        }
        val
    }

    fn r_rand(&mut self, s: i16) -> i16 {
        ((self.r_random() * s as f64).floor() as i64 % s as i64) as i16
    }

    fn permute_rotor(&mut self, base: usize) {
        let size = self.size;
        for j in 0..size {
            self.e_rotor[base + j] = j as u8;
        }
        let mut i = size;
        while i >= 2 {
            let q = self.r_rand(i as i16) as usize;
            i -= 1;
            let j = self.e_rotor[base + q];
            self.e_rotor[base + q] = self.e_rotor[base + i];
            self.e_rotor[base + i] = j;
            self.d_rotor[base + j as usize] = i as u8;
        }
        let e0 = self.e_rotor[base] as usize;
        self.d_rotor[base + e0] = 0;
    }

    fn init(&mut self) {
        self.seed = [self.key[0] as i32, self.key[1] as i32, self.key[2] as i32];
        for i in 0..self.rotors {
            self.positions[i] = 1;
            self.advances[i] = 1;
            for j in 0..self.size {
                self.e_rotor[i * self.size + j] = j as u8;
                self.d_rotor[i * self.size + j] = j as u8;
            }
        }
        for i in 0..self.rotors {
            self.positions[i] = self.r_rand(self.size as i16) as u8;
            self.advances[i] = (1 + 2 * self.r_rand((self.size / 2) as i16)) as u8;
            self.permute_rotor(i * self.size);
        }
        self.inited = true;
    }

    fn advance(&mut self) {
        for i in 0..self.rotors {
            let temp = self.positions[i] as usize + self.advances[i] as usize;
            self.positions[i] = (temp & self.size_mask) as u8;
            if temp >= self.size && i + 1 < self.rotors {
                self.positions[i + 1] = self.positions[i + 1].wrapping_add(1);
            }
        }
    }

    pub fn e_char(&mut self, p: u8) -> u8 {
        let mut tp = p;
        for i in 0..self.rotors {
            tp = self.e_rotor[i * self.size + ((self.positions[i] ^ tp) as usize & self.size_mask)];
        }
        self.advance();
        tp
    }

    pub fn d_char(&mut self, c: u8) -> u8 {
        let mut tc = c;
        for i in (0..self.rotors).rev() {
            tc = (self.positions[i] ^ self.d_rotor[i * self.size + tc as usize]) & self.size_mask as u8;
        }
        self.advance();
        tc
    }

    pub fn e_region(&mut self, data: &mut [u8]) {
        self.init();
        for b in data.iter_mut() {
            *b = self.e_char(*b);
        }
    }

    pub fn d_region(&mut self, data: &mut [u8]) {
        self.init();
        for b in data.iter_mut() {
            *b = self.d_char(*b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut r = Rotor::new(6, &default_key());
        let plain = b"the quick brown fox jumps over the lazy dog".to_vec();
        let mut data = plain.clone();
        r.e_region(&mut data);
        assert_ne!(data, plain);
        let mut back = data.clone();
        Rotor::new(6, &default_key()).d_region(&mut back);
        assert_eq!(back, plain);
    }

    #[test]
    fn key_layout_is_stable() {
        // Regression guard: the expanded key must stay bit-identical, otherwise
        // every payload silently fails to decode.
        let k = default_key();
        // seed*4 + (t + seed + f)*5 + "$@" + t*7 + f*2 + "&]`"
        // with seed = 13, t = 7 and f = 13 bytes.
        assert_eq!(k.len(), 13 * 4 + (7 + 13 + 13) * 5 + 2 + 7 * 7 + 13 * 2 + 3);
        assert_eq!(k.len(), 297);
        assert_eq!(&k[..13], b"j2h56ogodh3sk");
        assert!(k.ends_with(b"&]`"));
    }
}
