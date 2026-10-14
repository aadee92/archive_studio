//! Grouping embeddings into people.
//!
//! [`cluster`] is DBSCAN over cosine distance: a face with at least `min_faces` faces
//! (itself included) within `max_distance` is a core face, and core faces within reach of one
//! another form a person, along with the non-core faces they reach. Everything else is
//! unassigned. Requiring dense neighbourhoods keeps a single ambiguous face from chaining two
//! people together. It compares every pair (on all cores), so it suits up to some tens of
//! thousands of faces.
//!
//! [`nearest`] files a new face under an existing person without reclustering.

use crate::Embedding;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    /// Largest cosine distance at which two faces count as neighbours.
    pub max_distance: f32,
    /// Neighbours, counting the face itself, that make a face a core face.
    pub min_faces: usize,
}

impl Default for Params {
    /// Immich's defaults, also used with `buffalo_l`.
    fn default() -> Self {
        Self { max_distance: 0.5, min_faces: 3 }
    }
}

/// The person index of each embedding, numbered from 0 in order of first appearance, or `None`
/// for faces that belong to no group.
pub fn cluster(embeddings: &[Embedding], params: &Params) -> Vec<Option<usize>> {
    let n = embeddings.len();
    let neighbours_of = |i: usize| -> Vec<usize> {
        (0..n).filter(|&j| embeddings[i].distance(&embeddings[j]) <= params.max_distance).collect()
    };
    // The pairwise distances are the whole cost: spread them over the cores.
    let threads = std::thread::available_parallelism().map_or(1, |t| t.get()).min(n.div_ceil(256)).max(1);
    let chunk = n.div_ceil(threads).max(1);
    let neighbours: Vec<Vec<usize>> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..n)
            .step_by(chunk)
            .map(|lo| s.spawn(move || (lo..(lo + chunk).min(n)).map(neighbours_of).collect::<Vec<_>>()))
            .collect();
        workers.into_iter().flat_map(|w| w.join().unwrap()).collect()
    });
    let core = |i: usize| neighbours[i].len() >= params.min_faces;

    let mut labels = vec![None; n];
    let mut next = 0;
    for start in 0..n {
        if labels[start].is_some() || !core(start) {
            continue;
        }
        let label = next;
        next += 1;
        labels[start] = Some(label);
        let mut stack = vec![start];
        while let Some(i) = stack.pop() {
            for &j in &neighbours[i] {
                if labels[j].is_none() {
                    labels[j] = Some(label);
                    if core(j) {
                        stack.push(j);
                    }
                }
            }
        }
    }
    labels
}

/// The index and distance of the person in `people` (one representative embedding each, such
/// as an [`Embedding::mean`]) closest to `face`, if any is within `max_distance`.
pub fn nearest(face: &Embedding, people: &[Embedding], max_distance: f32) -> Option<(usize, f32)> {
    people
        .iter()
        .map(|p| face.distance(p))
        .enumerate()
        .filter(|&(_, d)| d <= max_distance)
        .min_by(|a, b| a.1.total_cmp(&b.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit vector at `deg` degrees in the plane of the first two axes.
    fn at(deg: f32) -> Embedding {
        let r = deg.to_radians();
        Embedding::new(vec![r.cos(), r.sin(), 0.0])
    }

    #[test]
    fn separates_dense_groups_and_leaves_outliers() {
        // cos 30° ≈ 0.87: distance 0.13 within a group. Groups 90° apart: distance 1.
        let faces = [at(0.0), at(90.0), at(10.0), at(100.0), at(20.0), at(95.0), at(200.0)];
        let labels = cluster(&faces, &Params::default());
        assert_eq!(labels, [Some(0), Some(1), Some(0), Some(1), Some(0), Some(1), None]);
    }

    #[test]
    fn a_pair_is_not_a_person_by_default() {
        assert_eq!(cluster(&[at(0.0), at(5.0)], &Params::default()), [None, None]);
        let pairs = Params { min_faces: 2, ..Params::default() };
        assert_eq!(cluster(&[at(0.0), at(5.0)], &pairs), [Some(0), Some(0)]);
    }

    #[test]
    fn nearest_respects_the_threshold() {
        let people = [at(0.0), at(90.0)];
        assert_eq!(nearest(&at(80.0), &people, 0.5).map(|(i, _)| i), Some(1));
        assert_eq!(nearest(&at(45.0), &people, 0.2), None);
    }

    #[test]
    fn embedding_bytes_round_trip() {
        let e = at(33.0);
        assert_eq!(Embedding::from_bytes(&e.to_bytes()), Some(e));
    }
}
