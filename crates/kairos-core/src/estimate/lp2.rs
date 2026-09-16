//! 二維線性規劃：在一組半平面 `a·x + b·y ≤ c` 的交集上最大化線性目標。
//!
//! 用頂點枚舉解：所有約束線兩兩相交、留下滿足全部約束的頂點、代入目標取極值。
//! O(n³)，但估計器的 n 只有一兩百，每幾分鐘解一次，正確性比速度重要。
//!
//! 前提：可行區域有界。估計器一定會加 |ρ| ≤ ρmax 這對約束，加上至少一筆樣本的
//! 帶狀區域，區域就有界；有界的非空凸多邊形一定有頂點，且線性目標的極值在頂點上。

/// 半平面 `a·x + b·y ≤ c`。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HalfPlane {
    pub a: f64,
    pub b: f64,
    pub c: f64,
}

impl HalfPlane {
    pub const fn new(a: f64, b: f64, c: f64) -> HalfPlane {
        HalfPlane { a, b, c }
    }

    fn satisfied_by(&self, x: f64, y: f64) -> bool {
        let lhs = self.a * x + self.b * y;
        // 容差：相對於約束的量級，再加一點絕對值，吃掉 f64 的累積誤差。
        let scale = lhs.abs().max(self.c.abs()).max(1.0);
        lhs <= self.c + scale * 1e-9
    }
}

/// 兩條線 `a₁x + b₁y = c₁`、`a₂x + b₂y = c₂` 的交點；平行時為 `None`。
fn intersect(p: &HalfPlane, q: &HalfPlane) -> Option<(f64, f64)> {
    let det = p.a * q.b - p.b * q.a;
    let scale = (p.a.abs() + p.b.abs()) * (q.a.abs() + q.b.abs());
    if det.abs() <= scale * 1e-12 {
        return None;
    }
    let x = (p.c * q.b - p.b * q.c) / det;
    let y = (p.a * q.c - p.c * q.a) / det;
    Some((x, y))
}

/// 可行區域的所有頂點（去重不做，重複頂點不影響極值）。空集合代表不可行。
pub fn feasible_vertices(constraints: &[HalfPlane]) -> Vec<(f64, f64)> {
    let mut vertices = Vec::new();
    for (i, p) in constraints.iter().enumerate() {
        for q in &constraints[i + 1..] {
            let Some((x, y)) = intersect(p, q) else { continue };
            if !x.is_finite() || !y.is_finite() {
                continue;
            }
            if constraints.iter().all(|h| h.satisfied_by(x, y)) {
                vertices.push((x, y));
            }
        }
    }
    vertices
}

/// 線性目標 `ox·x + oy·y` 在頂點集上的最小值與最大值。
pub fn extremes(vertices: &[(f64, f64)], ox: f64, oy: f64) -> Option<(f64, f64)> {
    let mut it = vertices.iter().map(|&(x, y)| ox * x + oy * y);
    let first = it.next()?;
    Some(it.fold((first, first), |(lo, hi), v| (lo.min(v), hi.max(v))))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 單位正方形 0 ≤ x ≤ 1、0 ≤ y ≤ 1。
    fn unit_square() -> Vec<HalfPlane> {
        vec![
            HalfPlane::new(1.0, 0.0, 1.0),
            HalfPlane::new(-1.0, 0.0, 0.0),
            HalfPlane::new(0.0, 1.0, 1.0),
            HalfPlane::new(0.0, -1.0, 0.0),
        ]
    }

    #[test]
    fn square_vertices_and_extremes() {
        let v = feasible_vertices(&unit_square());
        assert_eq!(v.len(), 4);
        assert_eq!(extremes(&v, 1.0, 0.0), Some((0.0, 1.0)));
        assert_eq!(extremes(&v, 1.0, 1.0), Some((0.0, 2.0)));
        assert_eq!(extremes(&v, -1.0, 0.0), Some((-1.0, 0.0)));
    }

    #[test]
    fn extra_cut_reduces_region() {
        let mut c = unit_square();
        // x + y ≤ 0.5
        c.push(HalfPlane::new(1.0, 1.0, 0.5));
        let v = feasible_vertices(&c);
        let (lo, hi) = extremes(&v, 1.0, 0.0).unwrap();
        assert_eq!(lo, 0.0);
        assert!((hi - 0.5).abs() < 1e-12);
    }

    #[test]
    fn infeasible_has_no_vertices() {
        let mut c = unit_square();
        // x ≥ 2
        c.push(HalfPlane::new(-1.0, 0.0, -2.0));
        assert!(feasible_vertices(&c).is_empty());
        assert_eq!(extremes(&[], 1.0, 0.0), None);
    }

    #[test]
    fn parallel_lines_are_skipped_not_crashed() {
        let c = vec![
            HalfPlane::new(1.0, 0.0, 1.0),
            HalfPlane::new(2.0, 0.0, 4.0), // 與第一條平行
            HalfPlane::new(0.0, 1.0, 1.0),
            HalfPlane::new(0.0, -1.0, 0.0),
            HalfPlane::new(-1.0, 0.0, 0.0),
        ];
        let v = feasible_vertices(&c);
        assert_eq!(extremes(&v, 1.0, 0.0), Some((0.0, 1.0)));
    }

    #[test]
    fn large_magnitudes_keep_tolerance_sane() {
        // 估計器的量級：θ 約 1e8 ns，ρ 約 1e5 ns/s，時間差約 1e3 s。
        let c = vec![
            HalfPlane::new(1.0, 1800.0, 1e8),
            HalfPlane::new(-1.0, -1800.0, -1e8 + 2e6),
            HalfPlane::new(1.0, 0.0, 1e8 + 5e6),
            HalfPlane::new(-1.0, 0.0, -(1e8 - 5e6)),
            HalfPlane::new(0.0, 1.0, 5e5),
            HalfPlane::new(0.0, -1.0, 5e5),
        ];
        let v = feasible_vertices(&c);
        assert!(!v.is_empty());
        let (lo, hi) = extremes(&v, 1.0, 0.0).unwrap();
        assert!(lo < hi);
    }
}
