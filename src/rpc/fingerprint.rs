// SPDX-License-Identifier: Apache-2.0
// Emoji alphabet and visual mapping adapted from Nitella.
// Source: https://github.com/ivere27/nitella
// Modified for anticrumb: hash full leaf certificate DER instead of SPKI,
// and expand Nitella's four-emoji mapping to sixteen emojis.

use sha2::{Digest, Sha256};

const EMOJIS: [&str; 64] = [
    "🐶", "🐱", "🐭", "🐹", "🐰", "🦊", "🐻", "🐼", "🐨", "🐯", "🦁", "🐮", "🐷", "🐸", "🐵", "🐔",
    "🐧", "🐦", "🐤", "🦆", "🦅", "🦉", "🦇", "🐺", "🐗", "🐴", "🦄", "🐝", "🐛", "🦋", "🐌", "🐞",
    "🐜", "🦟", "🦗", "🕷", "🦂", "🐢", "🐍", "🦎", "🦖", "🦕", "🐙", "🦑", "🦐", "🦞", "🦀", "🐡",
    "🐠", "🐟", "🐬", "🐳", "🐋", "🦈", "🐊", "🐅", "🐆", "🦓", "🦍", "🦧", "🐘", "🦛", "🦏", "🐪",
];

/// A visual summary of the leaf certificate's SHA-256 fingerprint.
/// These sixteen emojis carry 96 bits; machine verification uses all 256 bits.
pub fn emoji_fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)[..16]
        .iter()
        .map(|byte| EMOJIS[usize::from(*byte) % EMOJIS.len()])
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visual_mapping_expands_nitella_to_sixteen_emojis() {
        assert_eq!(
            emoji_fingerprint(b""),
            "🕷 🐠 🐰 🐭 🐗 🐘 🐛 🦅 🦄 🦧 🐋 🐨 🐴 🐡 🦓 🦂"
        );
        assert_eq!(
            emoji_fingerprint(b"abc"),
            "🦍 🐆 🦇 🐪 🐔 🐱 🐔 🐙 🐱 🐱 🐶 🐌 🦋 🦀 🦗 🕷"
        );
        assert_eq!(emoji_fingerprint(b"abc").split_whitespace().count(), 16);
    }
}
