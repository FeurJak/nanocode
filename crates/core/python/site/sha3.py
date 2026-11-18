from hashlib import sha3_224, sha3_256, sha3_384, sha3_512, shake_128, shake_256

__all__ = ['keccak_224', 'keccak_256', 'keccak_384', 'keccak_512',
           'sha3_224', 'sha3_256', 'sha3_384', 'sha3_512', 'shake_128', 'shake_256']

_MASK = (1 << 64) - 1
_ROUNDS = (
    0x0000000000000001, 0x0000000000008082, 0x800000000000808A, 0x8000000080008000,
    0x000000000000808B, 0x0000000080000001, 0x8000000080008081, 0x8000000000008009,
    0x000000000000008A, 0x0000000000000088, 0x0000000080008009, 0x000000008000000A,
    0x000000008000808B, 0x800000000000008B, 0x8000000000008089, 0x8000000000008003,
    0x8000000000008002, 0x8000000000000080, 0x000000000000800A, 0x800000008000000A,
    0x8000000080008081, 0x8000000000008080, 0x0000000080000001, 0x8000000080008008,
)
_ROTATIONS = (
    0, 1, 62, 28, 27,
    36, 44, 6, 55, 20,
    3, 10, 43, 25, 39,
    41, 45, 15, 21, 8,
    18, 2, 61, 56, 14,
)
_MOVES = tuple((x + 5 * y, y + 5 * ((2 * x + 3 * y) % 5)) for x in range(5) for y in range(5))


def _permute(lanes):
    mask = _MASK
    for constant in _ROUNDS:
        c = [lanes[x] ^ lanes[x + 5] ^ lanes[x + 10] ^ lanes[x + 15] ^ lanes[x + 20] for x in range(5)]
        for x in range(5):
            d = c[(x - 1) % 5] ^ (((c[(x + 1) % 5] << 1) | (c[(x + 1) % 5] >> 63)) & mask)
            for y in range(0, 25, 5):
                lanes[x + y] ^= d
        b = [0] * 25
        for source, target in _MOVES:
            value = lanes[source]
            shift = _ROTATIONS[source]
            b[target] = ((value << shift) | (value >> (64 - shift))) & mask if shift else value
        for y in range(0, 25, 5):
            row = b[y:y + 5]
            for x in range(5):
                lanes[x + y] = row[x] ^ (~row[(x + 1) % 5] & row[(x + 2) % 5])
        lanes[0] ^= constant


class _Keccak:
    name = 'keccak'

    def __init__(self, data=b'', **_):
        self._lanes = [0] * 25
        self._pending = b''
        if data:
            self.update(data)

    @property
    def block_size(self):
        return 200 - 2 * self.digest_size

    def update(self, data):
        if isinstance(data, str):
            raise TypeError('Strings must be encoded before hashing')
        data = self._pending + bytes(data)
        rate = self.block_size
        whole = len(data) - len(data) % rate
        for start in range(0, whole, rate):
            self._absorb(data[start:start + rate])
        self._pending = data[whole:]

    def _absorb(self, block):
        lanes = self._lanes
        for index in range(len(block) // 8):
            lanes[index] ^= int.from_bytes(block[8 * index:8 * index + 8], 'little')
        _permute(lanes)

    def copy(self):
        other = type(self)()
        other._lanes = list(self._lanes)
        other._pending = self._pending
        return other

    def digest(self):
        rate = self.block_size
        padded = bytearray(self._pending) + bytearray(rate - len(self._pending))
        padded[len(self._pending)] ^= 0x01
        padded[-1] ^= 0x80
        other = self.copy()
        other._absorb(bytes(padded))
        out = b''.join(lane.to_bytes(8, 'little') for lane in other._lanes[:rate // 8])
        return out[:self.digest_size]

    def hexdigest(self):
        return self.digest().hex()


def _variant(bits):
    return type('keccak_%d' % bits, (_Keccak,), {'digest_size': bits // 8, 'name': 'keccak_%d' % bits})


keccak_224 = _variant(224)
keccak_256 = _variant(256)
keccak_384 = _variant(384)
keccak_512 = _variant(512)
