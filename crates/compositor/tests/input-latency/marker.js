// Two colored anchors and complementary 16-bit rows reject stale, partial,
// cropped and incorrectly scaled markers. Values are sampled independently.
export function decodeMarker(rows, stride) {
  const pixel = (row, cell) =>
    rows[row].slice(
      Math.round((cell * 32 + 16) * stride) * 4,
      Math.round((cell * 32 + 16) * stride) * 4 + 3,
    );
  const first = pixel(0, 0),
    second = pixel(1, 0);
  if (
    !(
      first[0] > 180 &&
      first[1] < 70 &&
      first[2] > 180 &&
      second[0] < 70 &&
      second[1] > 180 &&
      second[2] > 180
    )
  )
    return null;
  let result = 0;
  for (let bit = 0; bit < 16; bit++) {
    const a = pixel(0, bit + 1),
      b = pixel(1, bit + 1);
    const white = (p) => p.length === 3 && p.every((v) => v > 180);
    const black = (p) => p.length === 3 && p.every((v) => v < 70);
    if (white(a) && black(b)) result |= 1 << bit;
    else if (!(black(a) && white(b))) return null;
  }
  return result;
}
