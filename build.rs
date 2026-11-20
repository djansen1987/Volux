use std::fs;
use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR must be set"));
    let icon_path = out.join("volux.ico");
    fs::write(&icon_path, volux_ico()).expect("could not generate Volux icon");

    winresource::WindowsResource::new()
        .set_icon(icon_path.to_string_lossy().as_ref())
        .set("ProductName", "Volux")
        .set("FileDescription", "Volux audio mixer")
        .set("CompanyName", "Volux")
        .compile()
        .expect("could not embed Windows resources");
}

fn volux_ico() -> Vec<u8> {
    const WIDTH: u32 = 32;
    const HEIGHT: u32 = 32;
    const COLORS: usize = (WIDTH * HEIGHT * 4) as usize;
    const MASK: usize = (WIDTH * HEIGHT / 8) as usize;

    let mut ico = Vec::with_capacity(22 + 40 + COLORS + MASK);
    ico.extend_from_slice(&0u16.to_le_bytes());
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.push(WIDTH as u8);
    ico.push(HEIGHT as u8);
    ico.push(0);
    ico.push(0);
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.extend_from_slice(&32u16.to_le_bytes());
    ico.extend_from_slice(&((40 + COLORS + MASK) as u32).to_le_bytes());
    ico.extend_from_slice(&22u32.to_le_bytes());

    ico.extend_from_slice(&40u32.to_le_bytes());
    ico.extend_from_slice(&(WIDTH as i32).to_le_bytes());
    ico.extend_from_slice(&(HEIGHT as i32 * 2).to_le_bytes());
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.extend_from_slice(&32u16.to_le_bytes());
    ico.extend_from_slice(&0u32.to_le_bytes());
    ico.extend_from_slice(&(COLORS as u32).to_le_bytes());
    ico.extend_from_slice(&0i32.to_le_bytes());
    ico.extend_from_slice(&0i32.to_le_bytes());
    ico.extend_from_slice(&0u32.to_le_bytes());
    ico.extend_from_slice(&0u32.to_le_bytes());

    for y in (0..HEIGHT).rev() {
        for x in 0..WIDTH {
            let is_letter = ((8..=12).contains(&x) && (8..=21).contains(&y) && x - 8 <= y - 7)
                || ((19..=23).contains(&x) && (8..=21).contains(&y) && 23 - x <= y - 7)
                || (11..=20).contains(&x) && (19..=24).contains(&y);
            let inset = !(3..WIDTH - 3).contains(&x) || !(3..HEIGHT - 3).contains(&y);
            if inset {
                ico.extend_from_slice(&[0, 0, 0, 0]);
            } else if is_letter {
                ico.extend_from_slice(&[255, 255, 255, 255]);
            } else {
                ico.extend_from_slice(&[255, 92, 113, 255]);
            }
        }
    }

    ico.resize(ico.len() + MASK, 0);
    ico
}
