// SPDX-License-Identifier: Apache-2.0
// Emoji alphabet and visual mapping adapted from Nitella.
// Source: https://github.com/ivere27/nitella
// Modified for anticrumb: hash full trust-anchor certificate DER instead of SPKI,
// and expand Nitella's four-emoji mapping to sixteen emojis.

use sha2::{Digest, Sha256};

const EMOJIS: [&str; 64] = [
    "🐶", "🐱", "🐭", "🐹", "🐰", "🦊", "🐻", "🐼", "🐨", "🐯", "🦁", "🐮", "🐷", "🐸", "🐵", "🐔",
    "🐧", "🐦", "🐤", "🦆", "🦅", "🦉", "🦇", "🐺", "🐗", "🐴", "🦄", "🐝", "🐛", "🦋", "🐌", "🐞",
    "🐜", "🦟", "🦗", "🕷", "🦂", "🐢", "🐍", "🦎", "🦖", "🦕", "🐙", "🦑", "🦐", "🦞", "🦀", "🐡",
    "🐠", "🐟", "🐬", "🐳", "🐋", "🦈", "🐊", "🐅", "🐆", "🦓", "🦍", "🦧", "🐘", "🦛", "🦏", "🐪",
];

/// A visual summary of the certificate's SHA-256 fingerprint.
/// These sixteen emojis carry 96 bits; machine verification uses all 256 bits.
pub fn emoji_fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)[..16]
        .iter()
        .map(|byte| EMOJIS[usize::from(*byte) % EMOJIS.len()])
        .collect::<Vec<_>>()
        .join(" ")
}

/// Short visual summary (first 4 emojis) from a SHA-256 certificate fingerprint hex string.
pub(crate) fn short_emoji_from_pin(pin_hex: &str) -> Option<String> {
    let pin = pin_hex.trim();
    if pin.len() < 8 {
        return None;
    }
    let mut out = String::new();
    for i in 0..4 {
        let hex = pin.get(i * 2..i * 2 + 2)?;
        let byte = u8::from_str_radix(hex, 16).ok()?;
        out.push_str(EMOJIS[usize::from(byte) % EMOJIS.len()]);
    }
    Some(out)
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
        assert_eq!(
            short_emoji_from_pin(&crate::domain::digest(b"")).as_deref(),
            Some("🕷🐠🐰🐭")
        );
        assert_eq!(
            short_emoji_from_pin(&crate::domain::digest(b"abc")).as_deref(),
            Some("🦍🐆🦇🐪")
        );
        assert_eq!(short_emoji_from_pin("short"), None);
        assert_eq!(short_emoji_from_pin("🦀🦀🦀🦀"), None);
        assert_eq!(short_emoji_from_pin("ab🦀def01"), None);
    }
}
