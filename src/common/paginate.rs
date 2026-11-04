//! Collects the pages of a Drive `*.list` call.

use std::cmp::min;

/// One page request: how many items to ask for and the token of the page to continue from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PageRequest {
    pub(crate) page_size: i32,
    pub(crate) page_token: Option<String>,
}

/// One page of a list response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Page<T> {
    pub(crate) items: Vec<T>,
    pub(crate) next_page_token: Option<String>,
}

/// Fetches pages until `max` items are collected or a page comes back without a
/// `nextPageToken`, and returns at most `max` items.
///
/// Each request asks for `max_page_size` items, or fewer when fewer are still wanted. Empty pages
/// that carry a `nextPageToken` are followed, as the API allows them before the end of the list.
/// With `max == 0` no request is made.
pub(crate) async fn collect_pages<T, E>(
    max: usize,
    max_page_size: usize,
    mut fetch_page: impl AsyncFnMut(PageRequest) -> Result<Page<T>, E>,
) -> Result<Vec<T>, E> {
    let mut items = Vec::new();
    let mut page_token = None;

    while items.len() < max {
        let page_size = min(max_page_size, max - items.len());
        let page = fetch_page(PageRequest {
            page_size: i32::try_from(page_size).unwrap_or(i32::MAX),
            page_token: page_token.take(),
        })
        .await?;

        items.extend(page.items);

        match page.next_page_token {
            Some(token) => page_token = Some(token),
            None => break,
        }
    }

    items.truncate(max);
    Ok(items)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::{collect_pages, Page, PageRequest};

    /// Serves `pages` in order and records every request.
    struct StubPages {
        pages: VecDeque<Page<u32>>,
        requests: Vec<PageRequest>,
    }

    impl StubPages {
        fn new(pages: impl IntoIterator<Item = (Vec<u32>, Option<&'static str>)>) -> Self {
            let pages = pages
                .into_iter()
                .map(|(items, token)| Page {
                    items,
                    next_page_token: token.map(String::from),
                })
                .collect();
            Self {
                pages,
                requests: Vec::new(),
            }
        }

        async fn collect(&mut self, max: usize, max_page_size: usize) -> Vec<u32> {
            collect_pages(max, max_page_size, async |request| {
                self.requests.push(request);
                self.pages.pop_front().ok_or("no more stub pages")
            })
            .await
            .expect("stub should have enough pages")
        }

        fn requests(&self) -> Vec<(i32, Option<&str>)> {
            self.requests
                .iter()
                .map(|r| (r.page_size, r.page_token.as_deref()))
                .collect()
        }
    }

    fn numbers(range: std::ops::Range<u32>) -> Vec<u32> {
        range.collect()
    }

    #[tokio::test]
    async fn follows_tokens_until_last_page() {
        let mut stub = StubPages::new([
            (numbers(0..100), Some("t1")),
            (numbers(100..200), Some("t2")),
            (numbers(200..230), None),
        ]);

        let items = stub.collect(usize::MAX, 100).await;

        assert_eq!(items, numbers(0..230));
        assert_eq!(
            stub.requests(),
            [(100, None), (100, Some("t1")), (100, Some("t2"))]
        );
    }

    #[tokio::test]
    async fn stops_at_max_and_shrinks_last_page() {
        let mut stub = StubPages::new([
            (numbers(0..100), Some("t1")),
            (numbers(100..150), Some("t2")),
        ]);

        let items = stub.collect(150, 100).await;

        assert_eq!(items, numbers(0..150));
        assert_eq!(stub.requests(), [(100, None), (50, Some("t1"))]);
    }

    #[tokio::test]
    async fn small_max_requests_one_small_page() {
        let mut stub = StubPages::new([(numbers(0..3), Some("t1"))]);

        let items = stub.collect(3, 100).await;

        assert_eq!(items, numbers(0..3));
        assert_eq!(stub.requests(), [(3, None)]);
    }

    #[tokio::test]
    async fn truncates_oversized_page() {
        let mut stub = StubPages::new([(numbers(0..10), Some("t1"))]);

        let items = stub.collect(4, 100).await;

        assert_eq!(items, numbers(0..4));
        assert_eq!(stub.requests(), [(4, None)]);
    }

    #[tokio::test]
    async fn follows_empty_pages_with_token() {
        let mut stub = StubPages::new([
            (Vec::new(), Some("t1")),
            (numbers(0..2), Some("t2")),
            (Vec::new(), None),
        ]);

        let items = stub.collect(usize::MAX, 100).await;

        assert_eq!(items, numbers(0..2));
        assert_eq!(
            stub.requests(),
            [(100, None), (100, Some("t1")), (100, Some("t2"))]
        );
    }

    #[tokio::test]
    async fn zero_max_makes_no_request() {
        let mut stub = StubPages::new([]);

        let items = stub.collect(0, 100).await;

        assert_eq!(items, Vec::<u32>::new());
        assert_eq!(stub.requests(), Vec::new());
    }

    #[tokio::test]
    async fn propagates_fetch_error() {
        let result: Result<Vec<u32>, &str> =
            collect_pages(10, 5, async |_| Err("request failed")).await;

        assert_eq!(result, Err("request failed"));
    }
}
