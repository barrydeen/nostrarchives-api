"""Minimal NIP-19 bech32 encoding, so review output shows npubs.

Hex pubkeys are unusable for a human reviewer — you cannot paste one into a
client to check whether an account is real. Encoding to npub is the difference
between a reviewable list and a wall of hex.

Only npub encoding is needed here; the Rust side (src/nip19.rs) handles decoding.
"""

from __future__ import annotations

CHARSET = "qpzry9x8gf2tvdw0s3jn54khce6mua7l"


def _polymod(values: list[int]) -> int:
    generator = [0x3B6A57B2, 0x26508E6D, 0x1EA119FA, 0x3D4233DD, 0x2A1462B3]
    chk = 1
    for v in values:
        top = chk >> 25
        chk = ((chk & 0x1FFFFFF) << 5) ^ v
        for i in range(5):
            chk ^= generator[i] if ((top >> i) & 1) else 0
    return chk


def _hrp_expand(hrp: str) -> list[int]:
    return [ord(c) >> 5 for c in hrp] + [0] + [ord(c) & 31 for c in hrp]


def _checksum(hrp: str, data: list[int]) -> list[int]:
    values = _hrp_expand(hrp) + data + [0, 0, 0, 0, 0, 0]
    polymod = _polymod(values) ^ 1
    return [(polymod >> 5 * (5 - i)) & 31 for i in range(6)]


def _convertbits(data: bytes, frombits: int, tobits: int, pad: bool = True) -> list[int]:
    acc = 0
    bits = 0
    ret = []
    maxv = (1 << tobits) - 1
    for value in data:
        acc = (acc << frombits) | value
        bits += frombits
        while bits >= tobits:
            bits -= tobits
            ret.append((acc >> bits) & maxv)
    if pad and bits:
        ret.append((acc << (tobits - bits)) & maxv)
    return ret


def encode_npub(pubkey_hex: str) -> str:
    """Encode a 64-char hex pubkey as an npub1... string."""
    if len(pubkey_hex) != 64:
        return pubkey_hex
    try:
        raw = bytes.fromhex(pubkey_hex)
    except ValueError:
        return pubkey_hex
    data = _convertbits(raw, 8, 5)
    combined = data + _checksum("npub", data)
    return "npub1" + "".join(CHARSET[d] for d in combined)
