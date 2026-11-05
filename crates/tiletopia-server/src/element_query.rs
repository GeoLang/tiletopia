use serde_json::{Map, Value};
use sqlx::{QueryBuilder, Sqlite};

const MAX_FILTER_CHARACTERS: usize = 65_536;
const MAX_FILTER_DEPTH: usize = 20;
const MAX_FILTER_CLAUSES: usize = 100;
const MAX_FILTER_OBJECTS: usize = 1000;
const MAX_ARRAY_ENTRIES: usize = 100;
const MAX_KEY_CHARACTERS: usize = 1024;
const MAX_LIKE_CHARACTERS: usize = 1024;
const MAX_OPERAND_CHARACTERS: usize = 32_000;
const MAX_AGGREGATION_KEYS: usize = 100;
const MAX_AGGREGATION_KEY_CHARACTERS: usize = 256;
const MAX_SEARCH_CHARACTERS: usize = 1024;
const MAX_SEARCH_TERMS: usize = 64;
const MAX_SEARCH_DEPTH: usize = 16;

fn shortened(text: &str) -> String {
    let start: String = text.chars().take(64).collect();
    format!("{start:?}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootField {
    Label,
    ClassFullName,
    Id,
    Storey,
}

impl RootField {
    fn named(field: &str) -> Result<Self, String> {
        match field {
            "@label" => Ok(Self::Label),
            "@classFullName" => Ok(Self::ClassFullName),
            "@id" => Ok(Self::Id),
            "@storey" => Ok(Self::Storey),
            other => Err(format!("unknown root field {}", shortened(other))),
        }
    }

    pub fn column(self) -> &'static str {
        match self {
            Self::Label => "e.name",
            Self::ClassFullName => "e.class",
            Self::Id => "e.global_id",
            Self::Storey => "e.storey",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    Text(String),
    Number(f64),
    Boolean(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparison {
    Greater,
    GreaterOrEqual,
    Less,
    LessOrEqual,
}

impl Comparison {
    fn sql(self) -> &'static str {
        match self {
            Self::Greater => " > ",
            Self::GreaterOrEqual => " >= ",
            Self::Less => " < ",
            Self::LessOrEqual => " <= ",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PropertyCondition {
    IsAbsent,
    IsPresent,
    Equal(Scalar),
    NotEqual(Scalar),
    In(Vec<Scalar>),
    Like(String),
    Compare(Comparison, f64),
}

#[derive(Debug, Clone, PartialEq)]
pub enum RootCondition {
    Equal(String),
    NotEqual(String),
    In(Vec<String>),
    Like(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ElementFilter {
    All(Vec<ElementFilter>),
    Any(Vec<ElementFilter>),
    Root {
        field: RootField,
        condition: RootCondition,
    },
    Property {
        key: String,
        condition: PropertyCondition,
    },
}

#[derive(Default)]
struct FilterCounts {
    clauses: usize,
    objects: usize,
}

fn checked_key(key: &str) -> Result<(), String> {
    if key.chars().count() > MAX_KEY_CHARACTERS {
        return Err(format!(
            "a filter key has more than {MAX_KEY_CHARACTERS} characters"
        ));
    }
    Ok(())
}

fn checked_text(text: &str) -> Result<String, String> {
    if text.chars().count() > MAX_OPERAND_CHARACTERS {
        return Err(format!(
            "a string operand has more than {MAX_OPERAND_CHARACTERS} characters"
        ));
    }
    Ok(text.to_string())
}

fn checked_like(operand: &Value) -> Result<String, String> {
    let Value::String(pattern) = operand else {
        return Err("$like takes a string".to_string());
    };
    if pattern.chars().count() > MAX_LIKE_CHARACTERS {
        return Err(format!(
            "a $like pattern has more than {MAX_LIKE_CHARACTERS} characters"
        ));
    }
    Ok(pattern.clone())
}

fn checked_array<'a>(operator: &str, operand: &'a Value) -> Result<&'a [Value], String> {
    let Value::Array(entries) = operand else {
        return Err(format!("{operator} takes an array"));
    };
    if entries.len() > MAX_ARRAY_ENTRIES {
        return Err(format!(
            "{operator} has more than {MAX_ARRAY_ENTRIES} entries"
        ));
    }
    Ok(entries)
}

fn scalar(operator: &str, operand: &Value) -> Result<Scalar, String> {
    match operand {
        Value::String(text) => Ok(Scalar::Text(checked_text(text)?)),
        Value::Bool(boolean) => Ok(Scalar::Boolean(*boolean)),
        Value::Number(number) => number
            .as_f64()
            .filter(|value| value.is_finite())
            .map(Scalar::Number)
            .ok_or_else(|| format!("{operator} takes a finite number")),
        _ => Err(format!("{operator} takes a string, number or Boolean")),
    }
}

fn finite_number(operator: &str, operand: &Value) -> Result<f64, String> {
    operand
        .as_f64()
        .filter(|value| value.is_finite())
        .ok_or_else(|| format!("{operator} takes a finite number"))
}

fn property_condition(operator: &str, operand: &Value) -> Result<PropertyCondition, String> {
    match operator {
        "$eq" if operand.is_null() => Ok(PropertyCondition::IsAbsent),
        "$ne" if operand.is_null() => Ok(PropertyCondition::IsPresent),
        "$eq" => Ok(PropertyCondition::Equal(scalar(operator, operand)?)),
        "$ne" => Ok(PropertyCondition::NotEqual(scalar(operator, operand)?)),
        "$in" => checked_array(operator, operand)?
            .iter()
            .map(|entry| scalar(operator, entry))
            .collect::<Result<Vec<_>, _>>()
            .map(PropertyCondition::In),
        "$like" => checked_like(operand).map(PropertyCondition::Like),
        "$gt" => Ok(PropertyCondition::Compare(
            Comparison::Greater,
            finite_number(operator, operand)?,
        )),
        "$gte" => Ok(PropertyCondition::Compare(
            Comparison::GreaterOrEqual,
            finite_number(operator, operand)?,
        )),
        "$lt" => Ok(PropertyCondition::Compare(
            Comparison::Less,
            finite_number(operator, operand)?,
        )),
        "$lte" => Ok(PropertyCondition::Compare(
            Comparison::LessOrEqual,
            finite_number(operator, operand)?,
        )),
        other => Err(format!("unknown operator {}", shortened(other))),
    }
}

fn root_text(field: &str, operator: &str, operand: &Value) -> Result<String, String> {
    let Value::String(text) = operand else {
        return Err(format!("{field} with {operator} takes a string"));
    };
    checked_text(text)
}

fn root_condition(field: &str, operator: &str, operand: &Value) -> Result<RootCondition, String> {
    match operator {
        "$eq" => root_text(field, operator, operand).map(RootCondition::Equal),
        "$ne" => root_text(field, operator, operand).map(RootCondition::NotEqual),
        "$in" => checked_array(operator, operand)?
            .iter()
            .map(|entry| root_text(field, operator, entry))
            .collect::<Result<Vec<_>, _>>()
            .map(RootCondition::In),
        "$like" => checked_like(operand).map(RootCondition::Like),
        "$gt" | "$gte" | "$lt" | "$lte" => {
            Err(format!("{field} does not support the operator {operator}"))
        }
        other => Err(format!("unknown operator {}", shortened(other))),
    }
}

fn key_conditions<T>(
    operand: &Value,
    mut condition: impl FnMut(&str, &Value) -> Result<T, String>,
) -> Result<Vec<T>, String> {
    let Value::Object(operators) = operand else {
        return Ok(vec![condition("$eq", operand)?]);
    };
    if operators.is_empty() {
        return Err("a property's operator object is empty".to_string());
    }
    operators
        .iter()
        .map(|(operator, value)| {
            if !operator.starts_with('$') {
                return Err(format!("expected an operator, got {}", shortened(operator)));
            }
            condition(operator, value)
        })
        .collect()
}

fn parse_clause(
    key: &str,
    operand: &Value,
    counts: &mut FilterCounts,
) -> Result<Vec<ElementFilter>, String> {
    checked_key(key)?;
    counts.clauses += 1;
    if counts.clauses > MAX_FILTER_CLAUSES {
        return Err(format!(
            "the filter has more than {MAX_FILTER_CLAUSES} property clauses"
        ));
    }
    if key.starts_with('@') {
        let field = RootField::named(key)?;
        return Ok(key_conditions(operand, |operator, value| {
            root_condition(key, operator, value)
        })?
        .into_iter()
        .map(|condition| ElementFilter::Root { field, condition })
        .collect());
    }
    Ok(key_conditions(operand, property_condition)?
        .into_iter()
        .map(|condition| ElementFilter::Property {
            key: key.to_string(),
            condition,
        })
        .collect())
}

fn parse_logical(
    operator: &str,
    operand: &Value,
    depth: usize,
    counts: &mut FilterCounts,
) -> Result<Vec<ElementFilter>, String> {
    if depth >= MAX_FILTER_DEPTH {
        return Err(format!(
            "the filter has more than {MAX_FILTER_DEPTH} logical nesting levels"
        ));
    }
    let entries = checked_array(operator, operand)?;
    if entries.is_empty() {
        return Err(format!("{operator} takes a non-empty array"));
    }
    entries
        .iter()
        .map(|entry| parse_object(entry, depth + 1, counts))
        .collect()
}

fn parse_entries(
    object: &Map<String, Value>,
    depth: usize,
    counts: &mut FilterCounts,
) -> Result<ElementFilter, String> {
    let mut conditions = Vec::new();
    for (key, operand) in object {
        match key.as_str() {
            "$and" => conditions.push(ElementFilter::All(parse_logical(
                key, operand, depth, counts,
            )?)),
            "$or" => conditions.push(ElementFilter::Any(parse_logical(
                key, operand, depth, counts,
            )?)),
            other if other.starts_with('$') => {
                return Err(format!("unknown operator {}", shortened(other)));
            }
            _ => conditions.extend(parse_clause(key, operand, counts)?),
        }
    }
    Ok(ElementFilter::All(conditions))
}

fn parse_object(
    value: &Value,
    depth: usize,
    counts: &mut FilterCounts,
) -> Result<ElementFilter, String> {
    let Value::Object(object) = value else {
        return Err("a filter is a JSON object".to_string());
    };
    counts.objects += 1;
    if counts.objects > MAX_FILTER_OBJECTS {
        return Err(format!(
            "the filter has more than {MAX_FILTER_OBJECTS} objects"
        ));
    }
    parse_entries(object, depth, counts)
}

pub fn parse_filter(raw: &str) -> Result<ElementFilter, String> {
    if raw.chars().count() > MAX_FILTER_CHARACTERS {
        return Err(format!(
            "the filter has more than {MAX_FILTER_CHARACTERS} characters"
        ));
    }
    let value: Value =
        serde_json::from_str(raw).map_err(|error| format!("the filter is not JSON: {error}"))?;
    parse_object(&value, 0, &mut FilterCounts::default())
}

fn push_joined(
    query: &mut QueryBuilder<Sqlite>,
    children: &[ElementFilter],
    separator: &str,
    when_empty: &str,
) {
    if children.is_empty() {
        query.push(when_empty);
        return;
    }
    query.push("(");
    for (index, child) in children.iter().enumerate() {
        if index > 0 {
            query.push(separator);
        }
        child.push_sql(query);
    }
    query.push(")");
}

fn push_root_condition(
    query: &mut QueryBuilder<Sqlite>,
    field: RootField,
    condition: &RootCondition,
) {
    query.push(field.column());
    match condition {
        RootCondition::Equal(text) => {
            query.push(" = ").push_bind(text.clone());
        }
        RootCondition::NotEqual(text) => {
            query.push(" <> ").push_bind(text.clone());
        }
        RootCondition::Like(pattern) => {
            query.push(" LIKE ").push_bind(pattern.clone());
        }
        RootCondition::In(texts) => {
            query.push(" IN (");
            let mut separated = query.separated(", ");
            for text in texts {
                separated.push_bind(text.clone());
            }
            query.push(")");
        }
    }
}

fn push_rows_with_key(
    query: &mut QueryBuilder<Sqlite>,
    key: &str,
    value_condition: impl FnOnce(&mut QueryBuilder<Sqlite>),
) {
    query
        .push("(SELECT element_row FROM properties WHERE key = ")
        .push_bind(key.to_string());
    value_condition(query);
    query.push(")");
}

fn push_scalar_equal(query: &mut QueryBuilder<Sqlite>, value: &Scalar) {
    match value {
        Scalar::Text(text) => {
            query.push(" AND value_text = ").push_bind(text.clone());
        }
        Scalar::Number(number) => {
            query.push(" AND value_number = ").push_bind(*number);
        }
        Scalar::Boolean(boolean) => {
            query.push(" AND value_boolean = ").push_bind(*boolean);
        }
    }
}

fn push_scalars_in(query: &mut QueryBuilder<Sqlite>, values: &[Scalar]) {
    query.push(" AND (");
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            query.push(" OR ");
        }
        match value {
            Scalar::Text(text) => {
                query.push("value_text = ").push_bind(text.clone());
            }
            Scalar::Number(number) => {
                query.push("value_number = ").push_bind(*number);
            }
            Scalar::Boolean(boolean) => {
                query.push("value_boolean = ").push_bind(*boolean);
            }
        }
    }
    query.push(")");
}

fn push_property_condition(
    query: &mut QueryBuilder<Sqlite>,
    key: &str,
    condition: &PropertyCondition,
) {
    match condition {
        PropertyCondition::IsAbsent => {
            query.push("e.element_row NOT IN ");
            push_rows_with_key(query, key, |_| {});
        }
        PropertyCondition::IsPresent => {
            query.push("e.element_row IN ");
            push_rows_with_key(query, key, |_| {});
        }
        PropertyCondition::Equal(value) => {
            query.push("e.element_row IN ");
            push_rows_with_key(query, key, |query| push_scalar_equal(query, value));
        }
        PropertyCondition::NotEqual(value) => {
            query.push("(e.element_row IN ");
            push_rows_with_key(query, key, |_| {});
            query.push(" AND e.element_row NOT IN ");
            push_rows_with_key(query, key, |query| push_scalar_equal(query, value));
            query.push(")");
        }
        PropertyCondition::In(values) if values.is_empty() => {
            query.push("0");
        }
        PropertyCondition::In(values) => {
            query.push("e.element_row IN ");
            push_rows_with_key(query, key, |query| push_scalars_in(query, values));
        }
        PropertyCondition::Like(pattern) => {
            query.push("e.element_row IN ");
            push_rows_with_key(query, key, |query| {
                query
                    .push(" AND value_text LIKE ")
                    .push_bind(pattern.clone());
            });
        }
        PropertyCondition::Compare(comparison, number) => {
            query.push("e.element_row IN ");
            push_rows_with_key(query, key, |query| {
                query
                    .push(" AND value_number")
                    .push(comparison.sql())
                    .push_bind(*number);
            });
        }
    }
}

impl ElementFilter {
    pub fn push_sql(&self, query: &mut QueryBuilder<Sqlite>) {
        match self {
            Self::All(children) => push_joined(query, children, " AND ", "1"),
            Self::Any(children) => push_joined(query, children, " OR ", "0"),
            Self::Root {
                condition: RootCondition::In(texts),
                ..
            } if texts.is_empty() => {
                query.push("0");
            }
            Self::Root { field, condition } => push_root_condition(query, *field, condition),
            Self::Property { key, condition } => push_property_condition(query, key, condition),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupBy {
    Root(RootField),
    Property(String),
}

pub fn parse_group_by(raw: &str) -> Result<GroupBy, String> {
    if raw.is_empty() {
        return Err("groupBy is empty".to_string());
    }
    if raw.chars().count() > MAX_KEY_CHARACTERS {
        return Err(format!(
            "groupBy has more than {MAX_KEY_CHARACTERS} characters"
        ));
    }
    if raw.starts_with('@') {
        return RootField::named(raw).map(GroupBy::Root);
    }
    Ok(GroupBy::Property(raw.to_string()))
}

pub fn parse_aggregation_keys(raw: &str) -> Result<Vec<String>, String> {
    let keys: Vec<String> = if raw.starts_with('[') {
        serde_json::from_str(raw)
            .map_err(|error| format!("aggregations is not a JSON array of strings: {error}"))?
    } else {
        raw.split(',').map(str::to_string).collect()
    };
    if keys.is_empty() || keys.len() > MAX_AGGREGATION_KEYS {
        return Err(format!(
            "aggregations takes 1 to {MAX_AGGREGATION_KEYS} property keys"
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for key in &keys {
        if key.is_empty() || key.chars().count() > MAX_AGGREGATION_KEY_CHARACTERS {
            return Err(format!(
                "an aggregations key must have 1 to {MAX_AGGREGATION_KEY_CHARACTERS} characters"
            ));
        }
        if !seen.insert(key) {
            return Err(format!("aggregations repeats {}", shortened(key)));
        }
    }
    Ok(keys)
}

#[derive(Debug, Clone, PartialEq)]
enum SearchToken {
    And,
    Or,
    Not,
    Open,
    Close,
    Phrase(String),
    Word(String),
}

fn ends_search_word(character: char) -> bool {
    character.is_whitespace() || matches!(character, '+' | '|' | '"' | '(' | ')')
}

fn tokenize_search(raw: &str) -> Result<Vec<SearchToken>, String> {
    let mut tokens = Vec::new();
    let mut characters = raw.chars().peekable();
    while let Some(&character) = characters.peek() {
        match character {
            whitespace if whitespace.is_whitespace() => {
                characters.next();
            }
            '+' => {
                characters.next();
                tokens.push(SearchToken::And);
            }
            '|' => {
                characters.next();
                tokens.push(SearchToken::Or);
            }
            '-' => {
                characters.next();
                tokens.push(SearchToken::Not);
            }
            '(' => {
                characters.next();
                tokens.push(SearchToken::Open);
            }
            ')' => {
                characters.next();
                tokens.push(SearchToken::Close);
            }
            '"' => {
                characters.next();
                let mut phrase = String::new();
                loop {
                    match characters.next() {
                        Some('"') => break,
                        Some('\\') => phrase.extend(characters.next()),
                        Some(inner) => phrase.push(inner),
                        None => return Err("a search phrase has no closing quote".to_string()),
                    }
                }
                tokens.push(SearchToken::Phrase(phrase));
            }
            _ => {
                let mut word = String::new();
                while let Some(&inner) = characters.peek() {
                    if ends_search_word(inner) {
                        break;
                    }
                    characters.next();
                    if inner == '\\' {
                        word.extend(characters.next());
                    } else {
                        word.push(inner);
                    }
                }
                tokens.push(SearchToken::Word(word));
            }
        }
    }
    Ok(tokens)
}

#[derive(Debug, Clone, PartialEq)]
enum SearchNode {
    Term { text: String, is_prefix: bool },
    And(Vec<SearchNode>),
    Or(Vec<SearchNode>),
    Not(Box<SearchNode>),
}

struct SearchParser {
    tokens: Vec<SearchToken>,
    position: usize,
    terms: usize,
}

impl SearchParser {
    fn peek(&self) -> Option<&SearchToken> {
        self.tokens.get(self.position)
    }

    fn take_term(&mut self, text: String, is_prefix: bool) -> Result<SearchNode, String> {
        self.terms += 1;
        if self.terms > MAX_SEARCH_TERMS {
            return Err(format!("the search has more than {MAX_SEARCH_TERMS} terms"));
        }
        if !text.chars().any(char::is_alphanumeric) {
            return Err(format!(
                "the search term {} has no letter or digit",
                shortened(&text)
            ));
        }
        Ok(SearchNode::Term { text, is_prefix })
    }

    fn parse_unary(&mut self, depth: usize) -> Result<SearchNode, String> {
        let Some(token) = self.tokens.get(self.position).cloned() else {
            return Err("the search ends where a term is expected".to_string());
        };
        self.position += 1;
        match token {
            SearchToken::Not => {
                if depth >= MAX_SEARCH_DEPTH {
                    return Err(format!(
                        "the search nests more than {MAX_SEARCH_DEPTH} levels"
                    ));
                }
                Ok(SearchNode::Not(Box::new(self.parse_unary(depth + 1)?)))
            }
            SearchToken::Open => {
                if depth >= MAX_SEARCH_DEPTH {
                    return Err(format!(
                        "the search nests more than {MAX_SEARCH_DEPTH} levels"
                    ));
                }
                let inner = self.parse_or(depth + 1)?;
                if self.peek() != Some(&SearchToken::Close) {
                    return Err("a search group has no closing parenthesis".to_string());
                }
                self.position += 1;
                Ok(inner)
            }
            SearchToken::Phrase(phrase) => self.take_term(phrase, false),
            SearchToken::Word(word) => match word.strip_suffix('*') {
                Some(prefix) => self.take_term(prefix.to_string(), true),
                None => self.take_term(word, false),
            },
            SearchToken::And | SearchToken::Or | SearchToken::Close => {
                Err("the search has an operator where a term is expected".to_string())
            }
        }
    }

    fn parse_and(&mut self, depth: usize) -> Result<SearchNode, String> {
        let mut children = vec![self.parse_unary(depth)?];
        while self.peek() == Some(&SearchToken::And) {
            self.position += 1;
            children.push(self.parse_unary(depth)?);
        }
        Ok(match children.len() {
            1 => children.remove(0),
            _ => SearchNode::And(children),
        })
    }

    fn parse_or(&mut self, depth: usize) -> Result<SearchNode, String> {
        // ion matches any word by default, so terms side by side are an OR
        let mut children = vec![self.parse_and(depth)?];
        loop {
            match self.peek() {
                Some(SearchToken::Or) => self.position += 1,
                Some(SearchToken::Close) | None => break,
                Some(_) => {}
            }
            children.push(self.parse_and(depth)?);
        }
        Ok(match children.len() {
            1 => children.remove(0),
            _ => SearchNode::Or(children),
        })
    }
}

const NOT_NEEDS_A_POSITIVE_TERM: &str =
    "NOT needs a positive term beside it under +, such as door + -wall";

fn render_search(node: &SearchNode) -> Result<String, String> {
    match node {
        SearchNode::Term { text, is_prefix } => {
            // fts5 treats a double-quoted string as one phrase, and a doubled quote is its only escape
            let quoted = text.replace('"', "\"\"");
            Ok(if *is_prefix {
                format!("\"{quoted}\" *")
            } else {
                format!("\"{quoted}\"")
            })
        }
        SearchNode::And(children) => {
            let mut positive = Vec::new();
            let mut negative = Vec::new();
            for child in children {
                match child {
                    SearchNode::Not(inner) => negative.push(render_search(inner)?),
                    other => positive.push(render_search(other)?),
                }
            }
            if positive.is_empty() {
                return Err(NOT_NEEDS_A_POSITIVE_TERM.to_string());
            }
            let mut rendered = format!("({})", positive.join(" AND "));
            for term in negative {
                rendered = format!("({rendered} NOT {term})");
            }
            Ok(rendered)
        }
        SearchNode::Or(children) => {
            let rendered = children
                .iter()
                .map(render_search)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(format!("({})", rendered.join(" OR ")))
        }
        SearchNode::Not(_) => Err(NOT_NEEDS_A_POSITIVE_TERM.to_string()),
    }
}

pub fn parse_search(raw: &str) -> Result<String, String> {
    if raw.chars().count() > MAX_SEARCH_CHARACTERS {
        return Err(format!(
            "the search has more than {MAX_SEARCH_CHARACTERS} characters"
        ));
    }
    // fts5 reads its query as a C string, so a NUL cuts it short mid-phrase
    if raw.chars().any(char::is_control) {
        return Err("the search holds a control character".to_string());
    }
    let mut parser = SearchParser {
        tokens: tokenize_search(raw)?,
        position: 0,
        terms: 0,
    };
    if parser.tokens.is_empty() {
        return Err("the search is empty".to_string());
    }
    let root = parser.parse_or(0)?;
    if parser.position != parser.tokens.len() {
        return Err("a search group has no opening parenthesis".to_string());
    }
    render_search(&root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested_filter(levels: usize) -> String {
        let mut filter = r#"{"A.B":1}"#.to_string();
        for _ in 0..levels {
            filter = format!(r#"{{"$and":[{filter}]}}"#);
        }
        filter
    }

    #[test]
    fn unknown_operators_are_refused_by_name() {
        for (filter, named) in [
            (r#"{"$nor":[{"A.B":1}]}"#, "$nor"),
            (r#"{"A.B":{"$regex":"x"}}"#, "$regex"),
            (r#"{"@label":{"$regex":"x"}}"#, "$regex"),
            (r#"{"@owner":"x"}"#, "@owner"),
            (r#"{"@label":{"$gt":1}}"#, "$gt"),
        ] {
            let error = parse_filter(filter).unwrap_err();
            assert!(error.contains(named), "{filter}: {error}");
        }
    }

    #[test]
    fn operands_of_the_wrong_type_are_refused() {
        for filter in [
            r#"{"A.B":{"$gt":"3"}}"#,
            r#"{"A.B":{"$like":3}}"#,
            r#"{"A.B":{"$in":"x"}}"#,
            r#"{"A.B":{"$in":[null]}}"#,
            r#"{"A.B":[1]}"#,
            r#"{"A.B":{}}"#,
            r#"{"A.B":{"C":1}}"#,
            r#"{"$and":[]}"#,
            r#"{"$and":[1]}"#,
            r#"{"@label":3}"#,
            r#"{"@label":null}"#,
            "[]",
            "not json",
        ] {
            assert!(parse_filter(filter).is_err(), "{filter}");
        }
    }

    #[test]
    fn ion_limits_are_enforced() {
        assert!(parse_filter(&nested_filter(MAX_FILTER_DEPTH)).is_ok());
        let error = parse_filter(&nested_filter(MAX_FILTER_DEPTH + 1)).unwrap_err();
        assert!(error.contains("nesting"), "{error}");

        let clauses = |count: usize| {
            let entries: Vec<String> = (0..count).map(|n| format!(r#""K{n}":1"#)).collect();
            format!("{{{}}}", entries.join(","))
        };
        assert!(parse_filter(&clauses(MAX_FILTER_CLAUSES)).is_ok());
        let error = parse_filter(&clauses(MAX_FILTER_CLAUSES + 1)).unwrap_err();
        assert!(error.contains("clauses"), "{error}");

        let entries = |count: usize| {
            let values = vec!["1"; count].join(",");
            format!(r#"{{"A.B":{{"$in":[{values}]}}}}"#)
        };
        assert!(parse_filter(&entries(MAX_ARRAY_ENTRIES)).is_ok());
        let error = parse_filter(&entries(MAX_ARRAY_ENTRIES + 1)).unwrap_err();
        assert!(error.contains("entries"), "{error}");

        let long_key = format!(r#"{{"{}":1}}"#, "k".repeat(MAX_KEY_CHARACTERS + 1));
        assert!(parse_filter(&long_key).unwrap_err().contains("key"));

        let long_like = format!(
            r#"{{"A.B":{{"$like":"{}"}}}}"#,
            "%".repeat(MAX_LIKE_CHARACTERS + 1)
        );
        assert!(parse_filter(&long_like).unwrap_err().contains("$like"));

        let long_operand = format!(r#"{{"A.B":"{}"}}"#, "x".repeat(MAX_OPERAND_CHARACTERS + 1));
        assert!(parse_filter(&long_operand).unwrap_err().contains("operand"));

        let long_filter = format!("{{{}}}", " ".repeat(MAX_FILTER_CHARACTERS));
        assert!(
            parse_filter(&long_filter)
                .unwrap_err()
                .contains("characters")
        );
    }

    #[test]
    fn filter_values_and_keys_never_reach_the_sql_text() {
        let filter = parse_filter(
            r#"{
                "x'; DROP TABLE elements; --": "y'; DROP TABLE properties; --",
                "@label": {"$like": "z'; DROP TABLE warnings; --"},
                "$or": [{"A.B": {"$in": ["w'; DROP TABLE build; --", 3, true]}}]
            }"#,
        )
        .unwrap();
        let mut query = QueryBuilder::<Sqlite>::new("SELECT 1 FROM elements e WHERE ");
        filter.push_sql(&mut query);
        let sql = query.sql();
        assert!(!sql.as_str().contains("DROP"), "{}", sql.as_str());
        assert!(!sql.as_str().contains('\''), "{}", sql.as_str());
    }

    #[test]
    fn group_by_names_a_property_or_a_known_root_field() {
        assert_eq!(
            parse_group_by("@storey"),
            Ok(GroupBy::Root(RootField::Storey))
        );
        assert_eq!(
            parse_group_by("Pset_A.B"),
            Ok(GroupBy::Property("Pset_A.B".to_string()))
        );
        assert!(parse_group_by("@nope").unwrap_err().contains("@nope"));
    }

    #[test]
    fn aggregation_keys_take_both_ion_forms() {
        assert_eq!(
            parse_aggregation_keys("A.B,C.D").unwrap(),
            vec!["A.B", "C.D"]
        );
        assert_eq!(
            parse_aggregation_keys(r#"["A,B","C.D"]"#).unwrap(),
            vec!["A,B", "C.D"]
        );
        assert!(parse_aggregation_keys("A,A").is_err());
        assert!(parse_aggregation_keys("A,").is_err());
        assert!(parse_aggregation_keys("[]").is_err());
        assert!(parse_aggregation_keys(&"k".repeat(257)).is_err());
    }

    #[test]
    fn search_operators_render_as_quoted_fts5_phrases() {
        assert_eq!(parse_search("wall").unwrap(), r#""wall""#);
        assert_eq!(parse_search("sear*").unwrap(), r#""sear" *"#);
        assert_eq!(parse_search("door wall").unwrap(), r#"("door" OR "wall")"#);
        assert_eq!(
            parse_search("door | wall").unwrap(),
            r#"("door" OR "wall")"#
        );
        assert_eq!(
            parse_search("door + wall").unwrap(),
            r#"("door" AND "wall")"#
        );
        assert_eq!(
            parse_search("door + -wall").unwrap(),
            r#"(("door") NOT "wall")"#
        );
        assert_eq!(
            parse_search(r#""fire door" + (green | blue)"#).unwrap(),
            r#"("fire door" AND ("green" OR "blue"))"#
        );
    }

    #[test]
    fn fts5_syntax_in_a_search_stays_inside_a_phrase() {
        assert_eq!(parse_search("name:secret").unwrap(), r#""name:secret""#);
        assert_eq!(parse_search("NEAR").unwrap(), r#""NEAR""#);
        assert_eq!(parse_search(r#"a\"b"#).unwrap(), r#""a""b""#);
        assert_eq!(parse_search(r#"x\+y"#).unwrap(), r#""x+y""#);
    }

    #[test]
    fn malformed_searches_are_refused() {
        for search in [
            "",
            "   ",
            "-wall",
            "door | -wall",
            "(door",
            "door)",
            "+ door",
            "door +",
            "\"door",
            "---",
            "a\0b",
        ] {
            assert!(parse_search(search).is_err(), "{search:?}");
        }
        let terms = vec!["a"; MAX_SEARCH_TERMS + 1].join(" ");
        assert!(parse_search(&terms).unwrap_err().contains("terms"));
        let nested = format!(
            "{}a{}",
            "(".repeat(MAX_SEARCH_DEPTH + 1),
            ")".repeat(MAX_SEARCH_DEPTH + 1)
        );
        assert!(parse_search(&nested).unwrap_err().contains("nests"));
        let long = "a".repeat(MAX_SEARCH_CHARACTERS + 1);
        assert!(parse_search(&long).unwrap_err().contains("characters"));
    }
}
