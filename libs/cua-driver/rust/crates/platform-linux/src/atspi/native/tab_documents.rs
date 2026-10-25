//! A bounded chrome-only look-ahead: Gecko exposes loaded background tabs as
//! VISIBLE documents, so ordinary DFS can exhaust the element cap before the
//! selected tab. Never enter a document during discovery (including iframes).
use super::*;

const PEEK_NODES: usize = 256;
const PEEK_TIMEOUT: Duration = Duration::from_millis(250);

pub(super) fn is_tab_document(role: &str) -> bool {
    role.eq_ignore_ascii_case("document web") || role.eq_ignore_ascii_case("document frame")
}

/// Search only the pending non-document branches of this same application
/// top-level. Unknown roles, oversized containers, errors and budget exhaustion
/// supply no evidence: the ordinary walk remains unchanged in that case.
pub(super) async fn find_showing(
    conn: &AccessibilityConnection,
    mut pending: Vec<RawObjectRef>,
    deadline: tokio::time::Instant,
) -> Option<RawObjectRef> {
    if pending.len() > PEEK_NODES {
        pending.drain(..pending.len() - PEEK_NODES);
    }
    let end = deadline.min(tokio::time::Instant::now() + PEEK_TIMEOUT);
    tokio::time::timeout_at(end, async {
        let mut seen = std::collections::HashSet::new();
        let mut remaining = PEEK_NODES;
        while let Some(oref) = pending.pop() {
            if remaining == 0 {
                break;
            }
            remaining -= 1;
            if !seen.insert((oref.name.clone(), oref.path.clone()))
                || is_web_process_bus(&oref.name)
            {
                continue;
            }
            // accessible_for and both raw child helpers enforce SANDBOX_GATE,
            // including references first discovered by this look-ahead.
            let Some(Ok(acc)) = call(accessible_for(conn, &oref)).await else {
                continue;
            };
            let Some(Ok(role)) = call(acc.get_role_name()).await else {
                continue;
            };
            if is_tab_document(&role) {
                if matches!(call(acc.get_state()).await, Some(Ok(s)) if s.contains(State::Showing))
                {
                    return Some(oref);
                }
                continue;
            }
            if is_document_role(&role) || role.is_empty() {
                continue;
            }
            let zconn = conn.connection();
            let Some(Ok(count)) = call(raw_child_count(zconn, &oref)).await else {
                continue;
            };
            if count <= 0 || count as usize > remaining.min(CHILD_ENUMERATION_CAP) {
                continue;
            }
            if let Some(Ok(children)) = call(raw_children(zconn, &oref)).await {
                // A bridge may change between ChildCount and GetChildren.
                // Bound the local queue as well as the number of addressed nodes.
                pending.extend(children.into_iter().take(remaining).rev());
                if pending.len() > remaining {
                    pending.drain(..pending.len() - remaining);
                }
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

/// Recheck the witness for every prune: a tab switch or minimized window must
/// not turn cached positive evidence into a reason to hide the selected page.
pub(super) async fn still_showing(conn: &AccessibilityConnection, witness: &RawObjectRef) -> bool {
    let Some(Ok(acc)) = call(accessible_for(conn, witness)).await else {
        return false;
    };
    matches!(call(acc.get_state()).await, Some(Ok(s)) if s.contains(State::Showing))
}

#[cfg(test)]
mod tests;
