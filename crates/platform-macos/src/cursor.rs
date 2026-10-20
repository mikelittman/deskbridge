use deskbridge_proto::CursorShape;

const SIZE: usize = 16;

/// Built-in arrow. The host sends it once; the viewer can draw it before that arrives.
pub fn arrow_shape() -> CursorShape {
    let mut bgra = vec![0u8; SIZE * SIZE * 4];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let outline = x == 0 || y == 0 || x == y || (x < 4 && y > 10) || (y < 4 && x > 10);
            let fill = x > 0 && y > 0 && x < y && x < 11 && y < 11;
            let (b, g, r, a) = if outline {
                (16, 16, 16, 255)
            } else if fill {
                (245, 245, 245, 255)
            } else {
                (0, 0, 0, 0)
            };
            let i = (y * SIZE + x) * 4;
            bgra[i] = b;
            bgra[i + 1] = g;
            bgra[i + 2] = r;
            bgra[i + 3] = a;
        }
    }
    CursorShape {
        width: SIZE as u16,
        height: SIZE as u16,
        hotspot_x: 1,
        hotspot_y: 1,
        bgra,
    }
}
