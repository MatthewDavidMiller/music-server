//! Typed access to a call's parameters.

use ms_http::Form;
use ms_library::Id;

use crate::error::{ApiError, ApiResult};

/// The largest page any list call returns.
pub const MAX_PAGE: usize = 500;

#[derive(Debug)]
pub struct Params<'a>(pub &'a Form);

impl<'a> Params<'a> {
    pub fn str(&self, name: &str) -> Option<&'a str> {
        self.0.get(name)
    }

    pub fn req(&self, name: &str) -> ApiResult<&'a str> {
        self.str(name).ok_or_else(|| ApiError::missing(name))
    }

    pub fn all(&self, name: &'a str) -> Vec<&'a str> {
        self.0.all(name).collect()
    }

    pub fn int(&self, name: &str) -> ApiResult<Option<i64>> {
        match self.str(name).map(str::trim) {
            None | Some("") => Ok(None),
            Some(v) => v.parse::<i64>().map(Some).map_err(|_| ApiError::bad_param(name)),
        }
    }

    /// A non-negative count with a default and a ceiling.
    pub fn count(&self, name: &str, default: usize, max: usize) -> ApiResult<usize> {
        match self.int(name)? {
            None => Ok(default.min(max)),
            Some(n) if n < 0 => Err(ApiError::bad_param(name)),
            Some(n) => Ok(usize::try_from(n).unwrap_or(usize::MAX).min(max)),
        }
    }

    pub fn offset(&self, name: &str) -> ApiResult<usize> {
        self.count(name, 0, 1_000_000)
    }

    pub fn bool(&self, name: &str) -> Option<bool> {
        match self.str(name)?.trim() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        }
    }

    pub fn id(&self, name: &str) -> ApiResult<Id> {
        Id::parse(self.req(name)?).map_err(|_| ApiError::not_found("Item"))
    }

    pub fn ids(&self, name: &'a str) -> ApiResult<Vec<Id>> {
        self.all(name)
            .into_iter()
            .map(|v| Id::parse(v).map_err(|_| ApiError::not_found("Item")))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_values() {
        let f = Form::new(
            [
                ("size", "20"),
                ("big", "99999"),
                ("neg", "-1"),
                ("bad", "x"),
                ("b", "true"),
                ("id", "tr-00112233445566778899aabb"),
                ("id", "nope"),
            ]
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        );
        let p = Params(&f);
        assert_eq!(p.count("size", 10, 500).unwrap(), 20);
        assert_eq!(p.count("big", 10, 500).unwrap(), 500);
        assert_eq!(p.count("missing", 10, 500).unwrap(), 10);
        assert_eq!(p.count("neg", 10, 500).unwrap_err().code, 10);
        assert_eq!(p.int("bad").unwrap_err().code, 10);
        assert_eq!(p.bool("b"), Some(true));
        assert!(p.id("id").is_ok());
        assert_eq!(p.ids("id").unwrap_err().code, 70);
        assert_eq!(p.req("none").unwrap_err().code, 10);
    }
}
