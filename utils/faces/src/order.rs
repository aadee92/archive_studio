//! Ordering faces for review so that each looks like the one before it.
//!
//! A face list sorted by score jumps around: a person as a child next to the same person in
//! old age. [`similarity_path`] instead lays the faces out as a short path through embedding
//! space, so a run of faces drifts gradually (child, teenager, adult, …) and a reviewer
//! compares each face only with its neighbours.
//!
//! The path is built greedily from one end (always step to the most similar face not yet
//! placed), then untangled with 2-opt moves: reversing a stretch of the path wherever that
//! shortens it. Reversals are limited to [`WINDOW`] faces, which removes the local zigzags
//! greedy paths leave while keeping the cost linear in the number of faces.

use crate::Embedding;

/// The longest stretch a 2-opt move reverses.
pub const WINDOW: usize = 64;
const MAX_PASSES: usize = 8;

/// The indices of `embeddings` in path order.
pub fn similarity_path(embeddings: &[Embedding]) -> Vec<usize> {
    let n = embeddings.len();
    if n <= 2 {
        return (0..n).collect();
    }
    let d = |i: usize, j: usize| embeddings[i].distance(&embeddings[j]);

    // Start at an extreme: the face least like an arbitrary one, so the path runs one way
    // rather than starting in the middle and having to double back.
    let start = (1..n).max_by(|&a, &b| d(0, a).total_cmp(&d(0, b))).unwrap_or(0);

    let mut path = Vec::with_capacity(n);
    let mut left: Vec<usize> = (0..n).filter(|&i| i != start).collect();
    path.push(start);
    while !left.is_empty() {
        let here = *path.last().unwrap();
        let (k, _) = left
            .iter()
            .enumerate()
            .min_by(|a, b| d(here, *a.1).total_cmp(&d(here, *b.1)))
            .unwrap();
        path.push(left.swap_remove(k));
    }

    // 2-opt on an open path: reversing path[i..=j] swaps the edges (i−1, i) and (j, j+1)
    // for (i−1, j) and (i, j+1); an end of the path has no edge to swap.
    for _ in 0..MAX_PASSES {
        let mut improved = false;
        for i in 0..n - 1 {
            for j in i + 1..(i + WINDOW).min(n) {
                let edge = |a: Option<usize>, b: Option<usize>| match (a, b) {
                    (Some(a), Some(b)) => d(path[a], path[b]),
                    _ => 0.0,
                };
                let (before, after) = (i.checked_sub(1), Some(j + 1).filter(|&k| k < n));
                let old = edge(before, Some(i)) + edge(Some(j), after);
                let new = edge(before, Some(j)) + edge(Some(i), after);
                if new < old - 1e-6 {
                    path[i..=j].reverse();
                    improved = true;
                }
            }
        }
        if !improved {
            break;
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(deg: f32) -> Embedding {
        let r = deg.to_radians();
        Embedding::new(vec![r.cos(), r.sin(), 0.0])
    }

    #[test]
    fn orders_points_along_an_arc() {
        // An arc of 0°..=150° in 10° steps, shuffled.
        let degs = [70.0, 0.0, 150.0, 30.0, 110.0, 50.0, 10.0, 130.0, 90.0, 20.0, 140.0, 60.0, 100.0, 40.0, 120.0, 80.0];
        let faces: Vec<_> = degs.iter().map(|&d| at(d)).collect();
        let mut got: Vec<f32> = similarity_path(&faces).into_iter().map(|i| degs[i]).collect();
        if got[0] > got[got.len() - 1] {
            got.reverse();
        }
        let want: Vec<f32> = (0..16).map(|k| k as f32 * 10.0).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn is_a_permutation() {
        let faces: Vec<_> = (0..200).map(|k| at((k * 37 % 360) as f32)).collect();
        let mut path = similarity_path(&faces);
        path.sort();
        assert_eq!(path, (0..200).collect::<Vec<_>>());
    }
}
