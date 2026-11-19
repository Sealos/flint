//! Page sizes and limits shared by list readers (the UI's one-shot lists and the MCP tools).

/// Default and maximum for a caller-supplied size.
#[derive(Debug, Clone, Copy)]
pub struct Bound {
    pub default: u32,
    pub max: u32,
}

impl Bound {
    /// The requested size, or the default, capped at the maximum. Zero means the default.
    pub fn clamp(self, requested: Option<u32>) -> u32 {
        match requested {
            None | Some(0) => self.default,
            Some(n) => n.min(self.max),
        }
    }
}

/// Whether there is more after this page, and the token to fetch it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageEnd {
    /// The server has more objects than were returned.
    pub more: bool,
    /// Pass back to fetch the next page. `None` when there is no next page, or when the server
    /// ignored `limit` and the page was cut here (then the caller has to narrow the query).
    pub continue_token: Option<String>,
}

/// Enforces `limit` on a page the server returned with `continue_token`. Some aggregated APIs
/// (metrics.k8s.io) ignore `limit`; their extra items are cut here, and since the server's token
/// (if any) would skip the items cut, it is dropped.
pub fn enforce_limit<T>(
    items: &mut Vec<T>,
    limit: Option<u32>,
    continue_token: Option<String>,
) -> PageEnd {
    let mut continue_token = continue_token.filter(|c| !c.is_empty());
    let mut more = continue_token.is_some();
    if let Some(limit) = limit
        && items.len() > limit as usize
    {
        items.truncate(limit as usize);
        more = true;
        continue_token = None;
    }
    PageEnd {
        more,
        continue_token,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_clamp() {
        let b = Bound {
            default: 10,
            max: 20,
        };
        assert_eq!(b.clamp(None), 10);
        assert_eq!(b.clamp(Some(0)), 10);
        assert_eq!(b.clamp(Some(5)), 5);
        assert_eq!(b.clamp(Some(50)), 20);
    }

    // covers UP-28
    #[test]
    fn limit_is_enforced_when_the_server_ignores_it() {
        let mut items = vec![1, 2, 3];
        let end = enforce_limit(&mut items, Some(2), Some("tok".into()));
        assert_eq!(items, [1, 2]);
        assert_eq!(
            end,
            PageEnd {
                more: true,
                continue_token: None
            }
        );
        let mut items = vec![1, 2];
        let end = enforce_limit(&mut items, Some(2), Some("tok".into()));
        assert_eq!(end.continue_token.as_deref(), Some("tok"));
        let end = enforce_limit(&mut items, None, Some(String::new()));
        assert_eq!(end, PageEnd::default());
    }
}
