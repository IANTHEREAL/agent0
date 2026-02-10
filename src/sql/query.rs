use sqlparser::ast::SetQuantifier;

pub fn is_set_quantifier_all(quantifier: &SetQuantifier) -> bool {
    matches!(quantifier, SetQuantifier::All)
}
