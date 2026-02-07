use sqlparser::ast::SetQuantifier;

pub fn is_set_quantifier_all(quantifier: &SetQuantifier) -> bool {
    matches!(quantifier, SetQuantifier::All)
}

pub fn reorder_by_indices<T: Clone>(data: &[T], indices: &[usize]) -> Vec<T> {
    indices.iter().map(|&idx| data[idx].clone()).collect()
}
