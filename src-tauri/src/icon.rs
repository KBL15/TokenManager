//! 托盘图标的位图绘制。
//!
//! 不用 Tauri 的 `set_title`（Windows 上文字挂在图标旁边，样式不可控，还会撑开
//! 通知区域），而是**把百分比直接画进图标位图**。
//!
//! 数字用内置的 3×5 点阵字体——十个 `const` 数组，零依赖、不需要字体文件。
//! 纯函数，可单测。

/// 图标边长。Windows 通知区域通常按 16/20/24/32 渲染，给 32 让系统缩放。
pub const SIZE: u32 = 32;

/// 3×5 点阵数字字体。
///
/// 每个数字 5 行，每行用低 3 位表示从左到右的三个像素
/// （`0b100` = 最左，`0b001` = 最右）。
#[rustfmt::skip]
const DIGITS: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111], // 0
    [0b010, 0b110, 0b010, 0b010, 0b111], // 1
    [0b111, 0b001, 0b111, 0b100, 0b111], // 2
    [0b111, 0b001, 0b111, 0b001, 0b111], // 3
    [0b101, 0b101, 0b111, 0b001, 0b001], // 4
    [0b111, 0b100, 0b111, 0b001, 0b111], // 5
    [0b111, 0b100, 0b111, 0b101, 0b111], // 6
    [0b111, 0b001, 0b001, 0b001, 0b001], // 7
    [0b111, 0b101, 0b111, 0b101, 0b111], // 8
    [0b111, 0b101, 0b111, 0b001, 0b111], // 9
];

/// 问号，用于百分比拿不到时。
#[rustfmt::skip]
const QUESTION: [u8; 5] = [0b111, 0b001, 0b011, 0b000, 0b010];

/// 取某个数字的第 row 行第 col 列是否点亮。
pub fn digit_pixel(digit: usize, row: usize, col: usize) -> bool {
    if digit > 9 || row > 4 || col > 2 {
        return false;
    }
    let bits = DIGITS[digit][row];
    let shift = 2 - col;
    (bits >> shift) & 1 == 1
}

/// 状态色：绿 / 黄 / 红。阈值与卡片进度条保持一致。
pub fn status_color(remaining_pct: Option<f64>) -> [u8; 3] {
    match remaining_pct {
        Some(p) if p < 20.0 => [0xE5, 0x3E, 0x3E], // 红
        Some(p) if p < 50.0 => [0xE8, 0xA3, 0x3D], // 黄
        Some(_) => [0x2E, 0xA0, 0x43],             // 绿
        None => [0x6B, 0x72, 0x80],                // 灰（无数据）
    }
}

/// 把百分比转成要画的字符。
///
/// - `Some(62.4)` → `"62"`
/// - `Some(100.0)` → `"99"`：99 是刻意封顶的。100 要三个字符，在小图标上会挤成
///   一团；而且托盘显示 99 与"额度充足"的语义并不冲突
/// - `None` → `"?"`
pub fn glyphs(remaining_pct: Option<f64>) -> Vec<Option<usize>> {
    match remaining_pct {
        None => vec![None],
        Some(p) => {
            let v = p.clamp(0.0, 99.0).round() as usize;
            let s = v.to_string();
            s.chars()
                .map(|c| c.to_digit(10).map(|d| d as usize))
                .collect()
        }
    }
}

/// 渲染图标，返回 RGBA 字节（长度 `SIZE*SIZE*4`）。
///
/// 底色是圆角方块 + 白字。圆角和边缘用 3×3 超采样做抗锯齿——比硬边缘好看很多，
/// 代价可以忽略。
pub fn render_icon(remaining_pct: Option<f64>) -> Vec<u8> {
    let color = status_color(remaining_pct);
    let mut buf = vec![0u8; (SIZE * SIZE * 4) as usize];

    // 1) 圆角方块底
    const MARGIN: f32 = 1.0;
    const RADIUS: f32 = 7.0;
    let x0 = MARGIN;
    let y0 = MARGIN;
    let x1 = SIZE as f32 - MARGIN;
    let y1 = SIZE as f32 - MARGIN;

    for py in 0..SIZE {
        for px in 0..SIZE {
            let a = rounded_rect_coverage(px as f32, py as f32, x0, y0, x1, y1, RADIUS);
            if a <= 0.0 {
                continue;
            }
            let idx = ((py * SIZE + px) * 4) as usize;
            // 稍降不透明度，避免在浅色任务栏上过于刺眼
            buf[idx] = color[0];
            buf[idx + 1] = color[1];
            buf[idx + 2] = color[2];
            buf[idx + 3] = (a * 235.0).round().clamp(0.0, 255.0) as u8;
        }
    }

    // 2) 白字
    let glyphs = glyphs(remaining_pct);
    draw_glyphs(&mut buf, &glyphs);

    buf
}

/// 圆角矩形的覆盖率（0.0–1.0），用 3×3 超采样抗锯齿。
fn rounded_rect_coverage(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> f32 {
    const SUB: u32 = 3;
    let mut hits = 0u32;

    for sy in 0..SUB {
        for sx in 0..SUB {
            let fx = px as f32 + (sx as f32 + 0.5) / SUB as f32;
            let fy = py as f32 + (sy as f32 + 0.5) / SUB as f32;
            if inside_rounded_rect(fx, fy, x0, y0, x1, y1, r) {
                hits += 1;
            }
        }
    }

    hits as f32 / (SUB * SUB) as f32
}

fn inside_rounded_rect(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> bool {
    if x < x0 || x > x1 || y < y0 || y > y1 {
        return false;
    }
    // 四个角分别用圆判定
    let cx = if x < x0 + r {
        x0 + r
    } else if x > x1 - r {
        x1 - r
    } else {
        return true;
    };
    let cy = if y < y0 + r {
        y0 + r
    } else if y > y1 - r {
        y1 - r
    } else {
        return true;
    };
    let (dx, dy) = (x - cx, y - cy);
    dx * dx + dy * dy <= r * r
}

/// 把点阵字符居中画到缓冲区上。
///
/// 缩放倍数按字符数自适应：1 个字符放大到 4 倍，2–3 个字符放大到 3 倍，
/// 保证在 32×32 里既填满又不出界。
fn draw_glyphs(buf: &mut [u8], glyphs: &[Option<usize>]) {
    if glyphs.is_empty() {
        return;
    }

    let scale: u32 = match glyphs.len() {
        1 => 4,
        2 => 3,
        _ => 2,
    };

    let glyph_w = 3 * scale;
    let gap = scale; // 字符间距
    let n = glyphs.len() as u32;
    let total_w = n * glyph_w + (n - 1) * gap;
    let total_h = 5 * scale;

    let start_x = (SIZE.saturating_sub(total_w)) / 2;
    let start_y = (SIZE.saturating_sub(total_h)) / 2;

    for (i, g) in glyphs.iter().enumerate() {
        let gx = start_x + i as u32 * (glyph_w + gap);
        match g {
            Some(d) => {
                for row in 0..5u32 {
                    for col in 0..3u32 {
                        if !digit_pixel(*d, row as usize, col as usize) {
                            continue;
                        }
                        fill_block(buf, gx + col * scale, start_y + row * scale, scale);
                    }
                }
            }
            None => {
                // 问号
                for row in 0..5u32 {
                    for col in 0..3u32 {
                        let bits = QUESTION[row as usize];
                        if (bits >> (2 - col)) & 1 == 0 {
                            continue;
                        }
                        fill_block(buf, gx + col * scale, start_y + row * scale, scale);
                    }
                }
            }
        }
    }
}

/// 往缓冲区填一个 `size×size` 的白色方块。
fn fill_block(buf: &mut [u8], x: u32, y: u32, size: u32) {
    for dy in 0..size {
        for dx in 0..size {
            let (px, py) = (x + dx, y + dy);
            if px >= SIZE || py >= SIZE {
                continue;
            }
            let idx = ((py * SIZE + px) * 4) as usize;
            buf[idx] = 0xFF;
            buf[idx + 1] = 0xFF;
            buf[idx + 2] = 0xFF;
            buf[idx + 3] = 0xFF;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_correct_buffer_size() {
        let b = render_icon(Some(62.0));
        assert_eq!(b.len(), (SIZE * SIZE * 4) as usize, "RGBA 每像素 4 字节");
    }

    /// 每个数字都得有像素，否则会画出空白。
    #[test]
    fn every_digit_has_lit_pixels() {
        for d in 0..10 {
            let count = (0..5)
                .flat_map(|r| (0..3).map(move |c| (r, c)))
                .filter(|(r, c)| digit_pixel(d, *r, *c))
                .count();
            assert!(count >= 6, "数字 {d} 只有 {count} 个亮点，可能画错");
        }
    }

    /// 点阵字体的形状断言——错一位就会画出别的数字。
    #[test]
    fn digit_shapes_are_correct() {
        // 1 的第一行只有中间亮
        assert!(!digit_pixel(1, 0, 0));
        assert!(digit_pixel(1, 0, 1));
        assert!(!digit_pixel(1, 0, 2));

        // 0 中间是空的
        assert!(!digit_pixel(0, 2, 1));

        // 8 中间是实的
        assert!(digit_pixel(8, 2, 1));

        // 7 除了第一行，其余只有右侧亮
        assert!(digit_pixel(7, 0, 0));
        assert!(!digit_pixel(7, 1, 0));
        assert!(digit_pixel(7, 1, 2));
    }

    #[test]
    fn digit_pixel_is_bounds_safe() {
        assert!(!digit_pixel(99, 0, 0));
        assert!(!digit_pixel(0, 99, 0));
        assert!(!digit_pixel(0, 0, 99));
    }

    #[test]
    fn glyphs_splits_digits() {
        assert_eq!(glyphs(Some(62.4)), vec![Some(6), Some(2)]);
        assert_eq!(glyphs(Some(7.6)), vec![Some(8)], "四舍五入");
        assert_eq!(glyphs(Some(0.0)), vec![Some(0)]);
    }

    /// 100% 画成 99，避免三个字符在小图标上挤成一团。
    #[test]
    fn glyphs_caps_at_ninety_nine() {
        assert_eq!(glyphs(Some(100.0)), vec![Some(9), Some(9)]);
        assert_eq!(glyphs(Some(99.4)), vec![Some(9), Some(9)]);
        // 超出范围的输入也要被夹住
        assert_eq!(glyphs(Some(150.0)), vec![Some(9), Some(9)]);
        assert_eq!(glyphs(Some(-5.0)), vec![Some(0)]);
    }

    #[test]
    fn glyphs_handles_missing_data() {
        assert_eq!(glyphs(None), vec![None], "拿不到百分比时画问号");
    }

    #[test]
    fn status_color_thresholds() {
        assert_eq!(status_color(Some(80.0)), [0x2E, 0xA0, 0x43], "充足是绿");
        assert_eq!(status_color(Some(50.0)), [0x2E, 0xA0, 0x43]);
        assert_eq!(status_color(Some(49.9)), [0xE8, 0xA3, 0x3D], "过半是黄");
        assert_eq!(status_color(Some(20.0)), [0xE8, 0xA3, 0x3D]);
        assert_eq!(status_color(Some(19.9)), [0xE5, 0x3E, 0x3E], "告急是红");
        assert_eq!(status_color(Some(0.0)), [0xE5, 0x3E, 0x3E]);
        assert_eq!(status_color(None), [0x6B, 0x72, 0x80], "无数据是灰");
    }

    /// 圆角判定：中心必在，外部必不在。
    #[test]
    fn rounded_rect_geometry() {
        assert!(inside_rounded_rect(16.0, 16.0, 1.0, 1.0, 31.0, 31.0, 7.0));
        // 四角外侧应被切掉
        assert!(!inside_rounded_rect(1.0, 1.0, 1.0, 1.0, 31.0, 31.0, 7.0));
        assert!(!inside_rounded_rect(31.0, 31.0, 1.0, 1.0, 31.0, 31.0, 7.0));
        // 边界外
        assert!(!inside_rounded_rect(0.5, 16.0, 1.0, 1.0, 31.0, 31.0, 7.0));
    }

    /// 四角必须是透明的，否则托盘上是方块而不是圆角。
    #[test]
    fn corners_are_transparent() {
        let b = render_icon(Some(62.0));
        let alpha_at = |x: u32, y: u32| b[((y * SIZE + x) * 4 + 3) as usize];

        assert_eq!(alpha_at(0, 0), 0, "左上角应完全透明");
        assert_eq!(alpha_at(SIZE - 1, 0), 0, "右上角应完全透明");
        assert_eq!(alpha_at(0, SIZE - 1), 0, "左下角应完全透明");
        assert_eq!(alpha_at(SIZE - 1, SIZE - 1), 0, "右下角应完全透明");
    }

    #[test]
    fn center_is_opaque() {
        let b = render_icon(Some(62.0));
        let alpha = b[((16 * SIZE + 16) * 4 + 3) as usize];
        assert!(alpha > 200, "中心应基本不透明，实际 {alpha}");
    }

    /// 数字要画得出来——中心区域得有白色像素。
    #[test]
    fn digits_are_drawn_white() {
        let b = render_icon(Some(62.0));
        let has_white = (0..SIZE)
            .flat_map(|y| (0..SIZE).map(move |x| (x, y)))
            .any(|(x, y)| {
                let i = ((y * SIZE + x) * 4) as usize;
                b[i] == 0xFF && b[i + 1] == 0xFF && b[i + 2] == 0xFF && b[i + 3] == 0xFF
            });
        assert!(has_white, "应能画出白色数字");
    }

    /// 拿不到数据时也要能画出问号，不能是空图标。
    #[test]
    fn unknown_state_still_draws_something() {
        let b = render_icon(None);
        let has_white = b.chunks(4).any(|p| p[0] == 0xFF && p[3] == 0xFF);
        assert!(has_white, "无数据时应画出问号");
    }

    /// 不同额度应产出不同位图——否则托盘图标不会随额度变化。
    #[test]
    fn different_percentages_produce_different_icons() {
        assert_ne!(render_icon(Some(80.0)), render_icon(Some(15.0)), "颜色该不同");
        assert_ne!(render_icon(Some(62.0)), render_icon(Some(38.0)), "数字该不同");
    }

    /// 数字不能画出图标边界。
    #[test]
    fn glyphs_stay_within_bounds() {
        for pct in [0.0, 5.0, 99.0] {
            let b = render_icon(Some(pct));
            assert_eq!(b.len(), (SIZE * SIZE * 4) as usize);
            // 最外一圈必须保持透明
            for i in 0..SIZE {
                assert_eq!(b[((0 * SIZE + i) * 4 + 3) as usize], 0, "顶边越界 pct={pct}");
                assert_eq!(
                    b[(((SIZE - 1) * SIZE + i) * 4 + 3) as usize],
                    0,
                    "底边越界 pct={pct}"
                );
            }
        }
    }
}