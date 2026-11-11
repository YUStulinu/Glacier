//! Content-defined chunking with FastCDC (Xia et al., USENIX ATC 2016): a gear rolling hash and
//! normalized chunking. Cut points depend only on the bytes near them, so an insertion moves the
//! boundaries around it but leaves the rest of the file's chunks unchanged.
//!
//! The gear table is derived from a per-repository secret seed (as restic and Borg randomise their
//! chunkers), so chunk sizes do not reveal known content to someone who sees only the repository.

use std::io::{self, Read};

pub struct Chunker {
    gear: [u64; 256],
    pub min: usize,
    pub avg: usize,
    pub max: usize,
    mask_s: u64,
    mask_l: u64,
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// `bits` ones in the high end of the word: the gear hash's high bits depend on the last 64
/// bytes, its low bits only on the last few.
fn mask(bits: u32) -> u64 {
    if bits == 0 { 0 } else { !0u64 << (64 - bits) }
}

impl Chunker {
    /// `avg` must be a power of two; min = avg / 4, max = avg * 4 (FastCDC's usual ratios).
    pub fn new(seed: u64, avg: usize) -> Chunker {
        assert!(avg.is_power_of_two() && avg >= 256);
        let mut s = seed;
        let mut gear = [0u64; 256];
        for g in gear.iter_mut() {
            *g = splitmix64(&mut s);
        }
        let bits = avg.trailing_zeros();
        // Normalization level 2: harder to cut before the average size, easier after it.
        Chunker { gear, min: avg / 4, avg, max: avg * 4, mask_s: mask(bits + 2), mask_l: mask(bits - 2) }
    }

    /// Length of the first chunk of `data` (all of it if `data` is shorter than `min`).
    pub fn cut(&self, data: &[u8]) -> usize {
        let mut n = data.len();
        if n <= self.min {
            return n;
        }
        if n > self.max {
            n = self.max;
        }
        let normal = self.avg.min(n);
        let mut fp = 0u64;
        let mut i = self.min;
        while i < normal {
            fp = (fp << 1).wrapping_add(self.gear[data[i] as usize]);
            if fp & self.mask_s == 0 {
                return i + 1;
            }
            i += 1;
        }
        while i < n {
            fp = (fp << 1).wrapping_add(self.gear[data[i] as usize]);
            if fp & self.mask_l == 0 {
                return i + 1;
            }
            i += 1;
        }
        n
    }

    /// Splits a stream, calling `f` with each chunk in order.
    pub fn split<R: Read>(&self, mut r: R, mut f: impl FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
        let mut buf = vec![0u8; self.max * 2];
        let mut len = 0;
        let mut eof = false;
        loop {
            while !eof && len < buf.len() {
                let k = r.read(&mut buf[len..])?;
                if k == 0 {
                    eof = true;
                } else {
                    len += k;
                }
            }
            if len == 0 {
                return Ok(());
            }
            let mut start = 0;
            // Cut while a whole maximum-size window is available (or the stream has ended).
            while start < len && (eof || len - start >= self.max) {
                let c = self.cut(&buf[start..len]);
                f(&buf[start..start + c])?;
                start += c;
            }
            buf.copy_within(start..len, 0);
            len -= start;
            if eof && len == 0 {
                return Ok(());
            }
        }
    }
}

/// Fixed-size chunks, for the evaluation's baseline.
pub fn fixed_split<R: Read>(mut r: R, size: usize, mut f: impl FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
    let mut buf = vec![0u8; size];
    loop {
        let mut len = 0;
        while len < size {
            let k = r.read(&mut buf[len..])?;
            if k == 0 {
                break;
            }
            len += k;
        }
        if len == 0 {
            return Ok(());
        }
        f(&buf[..len])?;
        if len < size {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(c: &Chunker, data: &[u8]) -> Vec<Vec<u8>> {
        let mut out = vec![];
        c.split(data, |x| {
            out.push(x.to_vec());
            Ok(())
        })
        .unwrap();
        out
    }

    fn random(n: usize, seed: u64) -> Vec<u8> {
        let mut r = fastrand::Rng::with_seed(seed);
        (0..n).map(|_| r.u8(..)).collect()
    }

    #[test]
    fn sizes_and_reassembly() {
        let c = Chunker::new(1, 8192);
        let data = random(2_000_000, 2);
        let cs = chunks(&c, &data);
        assert_eq!(cs.concat(), data);
        assert!(cs[..cs.len() - 1].iter().all(|x| x.len() >= c.min && x.len() <= c.max));
        let avg = data.len() / cs.len();
        // Normalized chunking keeps the mean near the target.
        assert!((6000..12000).contains(&avg), "average {avg}");
        // Small inputs and reader chunk boundaries do not change the result.
        assert_eq!(chunks(&c, &data[..100]), vec![data[..100].to_vec()]);
        let mut odd = vec![];
        struct Dribble<'a>(&'a [u8]);
        impl Read for Dribble<'_> {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                let n = self.0.len().min(b.len()).min(777);
                b[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }
        c.split(Dribble(&data), |x| {
            odd.push(x.to_vec());
            Ok(())
        })
        .unwrap();
        assert_eq!(odd, cs);
    }

    #[test]
    fn an_insertion_only_changes_nearby_chunks() {
        let c = Chunker::new(7, 4096);
        let a = random(500_000, 3);
        let mut b = a[..250_000].to_vec();
        b.extend_from_slice(b"a few inserted bytes");
        b.extend_from_slice(&a[250_000..]);
        let ca: std::collections::HashSet<_> = chunks(&c, &a).into_iter().collect();
        let cb = chunks(&c, &b);
        let shared = cb.iter().filter(|x| ca.contains(*x)).count();
        assert!(shared + 3 >= cb.len(), "{shared} of {} chunks shared", cb.len());
        // With fixed-size chunks every chunk after the insertion changes.
        let mut fa = std::collections::HashSet::new();
        fixed_split(&a[..], 4096, |x| {
            fa.insert(x.to_vec());
            Ok(())
        })
        .unwrap();
        let mut shared_fixed = 0;
        let mut nb = 0;
        fixed_split(&b[..], 4096, |x| {
            nb += 1;
            shared_fixed += fa.contains(x) as usize;
            Ok(())
        })
        .unwrap();
        assert!(shared_fixed < nb / 2 + 2);
    }

    #[test]
    fn seed_changes_boundaries() {
        let data = random(300_000, 9);
        assert_ne!(chunks(&Chunker::new(1, 4096), &data), chunks(&Chunker::new(2, 4096), &data));
    }
}
