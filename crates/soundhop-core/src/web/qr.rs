//! QR code rendering helpers for Web Broadcast (terminal UTF-8 half-blocks and RGBA matrix).

use qrcode::render::unicode;
use qrcode::{Color, QrCode};

use super::{WebError, WebResult};

/// Monochrome QR code matrix (`true` = dark module, `false` = light module).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrMatrix {
    /// Side length in modules (including any requested quiet zone border).
    pub width: usize,
    /// Row-major `width * width` module bitmap (`true` = dark).
    pub modules: Vec<bool>,
}

impl QrMatrix {
    /// Expand each module into a `scale x scale` pixel RGBA8 buffer (`[u8; 4]` per pixel).
    ///
    /// Returns `(pixel_width, rgba_bytes)` suitable for constructing an `egui::ColorImage`.
    pub fn to_rgba_colors(
        &self,
        scale: usize,
        dark: [u8; 4],
        light: [u8; 4],
    ) -> WebResult<(usize, Vec<u8>)> {
        let scale = scale.max(1);
        let pixel_width = self
            .width
            .checked_mul(scale)
            .ok_or_else(|| WebError::Qr("QR pixel width overflow".into()))?;
        let total_pixels = pixel_width
            .checked_mul(pixel_width)
            .ok_or_else(|| WebError::Qr("QR pixel buffer overflow".into()))?;
        let total_bytes = total_pixels
            .checked_mul(4)
            .ok_or_else(|| WebError::Qr("QR byte buffer overflow".into()))?;

        let mut rgba = Vec::with_capacity(total_bytes);
        for row in 0..self.width {
            let row_slice = &self.modules[row * self.width..(row + 1) * self.width];
            for _ in 0..scale {
                for &is_dark in row_slice {
                    let color = if is_dark { dark } else { light };
                    for _ in 0..scale {
                        rgba.extend_from_slice(&color);
                    }
                }
            }
        }
        Ok((pixel_width, rgba))
    }

    /// Expand each module into a black-on-white `scale x scale` RGBA8 buffer.
    pub fn to_rgba(&self, scale: usize) -> WebResult<(usize, Vec<u8>)> {
        self.to_rgba_colors(scale, [0, 0, 0, 255], [255, 255, 255, 255])
    }
}

/// Render `payload` as a compact UTF-8 half-block QR code for terminal / TUI display.
pub fn render_terminal_qr(payload: &str) -> WebResult<String> {
    let code = QrCode::new(payload.as_bytes())
        .map_err(|e| WebError::Qr(format!("failed to encode QR payload: {e}")))?;
    Ok(code.render::<unicode::Dense1x2>().quiet_zone(true).build())
}

/// Encode `payload` into a boolean [`QrMatrix`] with `quiet_zone_modules` white border on each side.
pub fn encode_qr_matrix(payload: &str, quiet_zone_modules: usize) -> WebResult<QrMatrix> {
    let code = QrCode::new(payload.as_bytes())
        .map_err(|e| WebError::Qr(format!("failed to encode QR payload: {e}")))?;
    let raw_width = code.width();
    let colors = code.to_colors();
    let total_width = raw_width
        .checked_add(quiet_zone_modules.saturating_mul(2))
        .ok_or_else(|| WebError::Qr("QR matrix width overflow".into()))?;
    let total_modules = total_width
        .checked_mul(total_width)
        .ok_or_else(|| WebError::Qr("QR matrix size overflow".into()))?;

    let mut modules = vec![false; total_modules];
    for y in 0..raw_width {
        for x in 0..raw_width {
            let is_dark = matches!(colors[y * raw_width + x], Color::Dark);
            if is_dark {
                let dst_y = y + quiet_zone_modules;
                let dst_x = x + quiet_zone_modules;
                modules[dst_y * total_width + dst_x] = true;
            }
        }
    }

    Ok(QrMatrix {
        width: total_width,
        modules,
    })
}

/// Encode `payload` directly into an RGBA8 pixel buffer `(pixel_width, rgba_bytes)`.
pub fn encode_qr_rgba(
    payload: &str,
    scale: usize,
    quiet_zone_modules: usize,
) -> WebResult<(usize, Vec<u8>)> {
    let matrix = encode_qr_matrix(payload, quiet_zone_modules)?;
    matrix.to_rgba(scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_qr_renders_utf8_half_blocks() {
        let url = "http://192.168.1.42:4717/#t=0123456789abcdef0123456789abcdef";
        let rendered = render_terminal_qr(url).expect("QR render succeeds");
        assert!(!rendered.is_empty());
        assert!(rendered.lines().count() >= 10);
        assert!(
            rendered.chars().any(|c| matches!(c, '▀' | '▄' | '█')),
            "expected UTF-8 half-block characters in rendered QR"
        );
    }

    #[test]
    fn matrix_respects_quiet_zone_and_rgba_dimensions() {
        let url = "http://localhost:4717/#t=deadbeef";
        let qz = 2;
        let matrix = encode_qr_matrix(url, qz).expect("matrix encode succeeds");
        assert!(matrix.width > qz * 2);
        assert_eq!(matrix.modules.len(), matrix.width * matrix.width);
        assert!(matrix.modules.iter().any(|&m| m));
        assert!(matrix.modules.iter().any(|&m| !m));

        // All outer quiet-zone border rows/columns must be light (`false`).
        for y in 0..matrix.width {
            for x in 0..matrix.width {
                if x < qz || y < qz || x >= matrix.width - qz || y >= matrix.width - qz {
                    assert!(
                        !matrix.modules[y * matrix.width + x],
                        "quiet zone module ({x}, {y}) should be light"
                    );
                }
            }
        }

        let scale = 4;
        let (px_w, rgba) = matrix.to_rgba(scale).expect("rgba conversion succeeds");
        assert_eq!(px_w, matrix.width * scale);
        assert_eq!(rgba.len(), px_w * px_w * 4);
        // Top-left corner pixel lies in quiet zone -> opaque white.
        assert_eq!(&rgba[0..4], &[255, 255, 255, 255]);
    }

    #[test]
    fn encode_qr_rgba_matches_matrix_conversion() {
        let payload = "http://10.0.0.5:4717/";
        let (w1, buf1) = encode_qr_rgba(payload, 3, 1).expect("direct rgba");
        let (w2, buf2) = encode_qr_matrix(payload, 1)
            .expect("matrix")
            .to_rgba(3)
            .expect("to_rgba");
        assert_eq!(w1, w2);
        assert_eq!(buf1, buf2);
    }

    #[test]
    fn oversized_qr_payload_returns_error() {
        let huge = "a".repeat(8000);
        let err = render_terminal_qr(&huge).unwrap_err();
        assert!(matches!(err, WebError::Qr(_)));
    }
}
