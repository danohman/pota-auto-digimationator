// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! Maidenhead grid squares and great-circle headings.

/// Center of a Maidenhead locator (4 or 6 characters, or longer; extra pairs are used), as
/// (latitude, longitude) in degrees. None when it is not a valid locator.
pub fn grid_center(grid: &str) -> Option<(f64, f64)> {
    let g: Vec<char> = grid.trim().chars().collect();
    if g.len() < 4 || g.len() % 2 != 0 {
        return None;
    }
    let letter = |c: char, max: u8| {
        let c = c.to_ascii_uppercase();
        (c.is_ascii_uppercase() && (c as u8 - b'A') < max).then(|| (c as u8 - b'A') as f64)
    };
    let digit = |c: char| c.to_digit(10).map(|d| d as f64);
    let mut lon = letter(g[0], 18)? * 20.0 - 180.0;
    let mut lat = letter(g[1], 18)? * 10.0 - 90.0;
    lon += digit(g[2])? * 2.0;
    lat += digit(g[3])?;
    let (mut w, mut h) = (2.0, 1.0); // size of the square so far
    let mut i = 4;
    while i < g.len() {
        if i % 4 == 0 {
            w /= 24.0;
            h /= 24.0;
            lon += letter(g[i], 24)? * w;
            lat += letter(g[i + 1], 24)? * h;
        } else {
            w /= 10.0;
            h /= 10.0;
            lon += digit(g[i])? * w;
            lat += digit(g[i + 1])? * h;
        }
        i += 2;
    }
    Some((lat + h / 2.0, lon + w / 2.0))
}

/// Initial great-circle heading from `from` to `to` (both (lat, lon) degrees), 0..360 true north.
pub fn bearing(from: (f64, f64), to: (f64, f64)) -> f64 {
    let (p1, p2) = (from.0.to_radians(), to.0.to_radians());
    let dl = (to.1 - from.1).to_radians();
    let y = dl.sin() * p2.cos();
    let x = p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos();
    (y.atan2(x).to_degrees() + 360.0) % 360.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_centers() {
        let (la, lo) = grid_center("EM80").unwrap();
        assert!((la - 30.5).abs() < 1e-9 && (lo - -83.0).abs() < 1e-9);
        let (la, lo) = grid_center("FN31pr").unwrap();
        assert!((la - 41.7292).abs() < 0.001 && (lo - -72.7083).abs() < 0.001);
        assert!(grid_center("XX").is_none() && grid_center("ZZ99").is_none() && grid_center("EM8").is_none());
    }

    #[test]
    fn headings() {
        assert!((bearing((0.0, 0.0), (10.0, 0.0)) - 0.0).abs() < 1e-6);
        assert!((bearing((0.0, 0.0), (0.0, 10.0)) - 90.0).abs() < 1e-6);
        assert!((bearing((10.0, 0.0), (0.0, 0.0)) - 180.0).abs() < 1e-6);
        assert!((bearing((0.0, 10.0), (0.0, 0.0)) - 270.0).abs() < 1e-6);
        // New York to London is about 51 degrees
        let b = bearing((40.71, -74.0), (51.5, -0.12));
        assert!((b - 51.2).abs() < 1.0, "{b}");
    }
}
