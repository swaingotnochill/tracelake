/// CRC-32 (IEEE, reflected) of `bytes`. Under Kani a bitwise reference
/// implementation replaces `crc32fast`, whose runtime CPU-feature detection is
/// not executable by the model checker; both compute the same standard
/// checksum.
pub(crate) fn checksum(bytes: &[u8]) -> u32 {
    #[cfg(kani)]
    {
        let mut crc: u32 = 0xFFFF_FFFF;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                let mask = u32::from(crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }
    #[cfg(not(kani))]
    {
        crc32fast::hash(bytes)
    }
}
