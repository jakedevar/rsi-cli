/// Compute the cosine similarity between two f32 vectors.
///
/// Returns a value in [-1.0, 1.0] where 1.0 means identical direction,
/// 0.0 means orthogonal, and -1.0 means opposite direction.
///
/// Returns 0.0 if either vector is empty or has zero magnitude.
/// If vectors differ in length, only the shorter prefix is compared.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    let len = a.len().min(b.len());
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;

    for i in 0..len {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }

    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }

    dot / (norm_a.sqrt() * norm_b.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_empty_vectors() {
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
    }

    #[test]
    fn test_cosine_one_empty() {
        assert_eq!(cosine_similarity(&[], &[1.0, 2.0]), 0.0);
        assert_eq!(cosine_similarity(&[1.0, 2.0], &[]), 0.0);
    }

    #[test]
    fn test_cosine_zero_vectors() {
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[0.0, 0.0]), 0.0);
    }

    #[test]
    fn test_cosine_identical_unit() {
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_opposite_unit() {
        assert!((cosine_similarity(&[1.0, 0.0], &[-1.0, 0.0]) - (-1.0)).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_orthogonal() {
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_45_degrees() {
        let sim = cosine_similarity(&[1.0, 0.0], &[1.0, 1.0]);
        let expected = 1.0 / 2.0f32.sqrt();
        assert!((sim - expected).abs() < 1e-5);
    }

    #[test]
    fn test_cosine_different_lengths() {
        let sim = cosine_similarity(&[1.0, 0.0, 0.0], &[1.0, 0.0]);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_single_element() {
        assert!((cosine_similarity(&[3.0], &[3.0]) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_realistic_embeddings() {
        // Two 384-dim vectors: one filled with 1.0, one filled with 0.5
        // cos(a, b) = sum(1.0*0.5) / (sqrt(sum(1.0^2)) * sqrt(sum(0.5^2)))
        // = 384*0.5 / (sqrt(384) * sqrt(384*0.25)) = 192 / (sqrt(384) * sqrt(96))
        // = 192 / sqrt(384*96) = 192 / sqrt(36864) = 192 / 192 = 1.0
        let a = vec![1.0f32; 384];
        let b = vec![0.5f32; 384];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn test_cosine_one_zero_magnitude() {
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 2.0]), 0.0);
    }

    #[test]
    fn test_cosine_negative_values() {
        assert!((cosine_similarity(&[-1.0, -2.0], &[-1.0, -2.0]) - 1.0).abs() < 1e-6);
    }
}
