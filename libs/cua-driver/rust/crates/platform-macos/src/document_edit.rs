//! Keep a document's edited state in step with accessibility text writes.
//!
//! An `AXValue` write, or an `AXSelectedText` insert, changes an `NSTextView`'s
//! text without going through its edit model: no undo entry is registered and
//! the `NSDocument` is not marked edited. TextEdit then never autosaves the
//! change, and closing or saving keeps the old content on disk (Bench v2
//! DS-07). Key events do go through the edit model.
//!
//! AX does not provide an atomic retained-recipient keyboard transaction.
//! A focus readback followed by pid-routed input can still race focus changes,
//! especially between an inserted space and its compensating delete. Until
//! that transaction can be proven, document marking fails closed without keys.

use crate::ax::bindings::{copy_element_attr, copy_string_attr, AXUIElementRef};

/// What the document's edited mark showed after an AX text write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentEdited {
    /// The element is not in a document window, so there is nothing to save.
    NotADocument,
    /// The driver sent a net-zero key edit and the text did not move.
    Marked,
    /// The key edit could not be sent to this window, or the text moved. A
    /// save may keep the old content.
    NotMarked,
}

impl DocumentEdited {
    /// A sentence for the tool result, or nothing when there is nothing to say.
    pub fn note(self) -> &'static str {
        match self {
            DocumentEdited::NotADocument => "",
            DocumentEdited::Marked => {
                " A net-zero key edit (space, backspace) was sent so the document registers the \
                 change and saves it; an accessibility write alone does not."
            }
            DocumentEdited::NotMarked => {
                " Warning: the document edited state could not be verified, so saving or closing it may keep \
                 the old content. Type the text with key events instead (click the field, then \
                 type_text), or edit it in the foreground."
            }
        }
    }

    /// The structured value, `None` when the element is not in a document.
    pub fn structured(self) -> Option<bool> {
        match self {
            DocumentEdited::NotADocument => None,
            DocumentEdited::NotMarked => Some(false),
            DocumentEdited::Marked => Some(true),
        }
    }
}

/// Whether `element` sits in a document window (its window has an
/// `AXDocument` URL).
///
/// # Safety
/// `element` must be a live `AXUIElementRef`.
unsafe fn in_document_window(element: AXUIElementRef) -> bool {
    let Some(window) = copy_element_attr(element, "AXWindow") else {
        return false;
    };
    let document = copy_string_attr(window, "AXDocument");
    core_foundation::base::CFRelease(window as _);
    document.is_some_and(|url| !url.is_empty())
}

/// After an AX text write on `element` (in `pid`) that changed its value, make
/// sure its document is marked edited. See the module docs.
///
/// # Safety
/// `element` must be a live `AXUIElementRef` for the duration of the call.
/// `caret_to_end` is retained for callers, but no selection or keyboard edit
/// is attempted until a precise recipient and compensation can be proven.
pub unsafe fn ensure_marked_after_ax_write(
    pid: i32,
    element: AXUIElementRef,
    caret_to_end: bool,
) -> DocumentEdited {
    if !in_document_window(element) {
        return DocumentEdited::NotADocument;
    }
    // Do not infer a successful native edit from unchanged AXValue: keys
    // could have reached a sibling. There is no safe compensation transaction
    // here, so preserve the AX write and report the missing edited mark.
    let _ = (pid, caret_to_end);
    DocumentEdited::NotMarked
}

#[cfg(test)]
mod tests {
    use super::DocumentEdited;

    #[test]
    fn only_a_document_left_unedited_gets_a_warning() {
        assert_eq!(DocumentEdited::NotADocument.note(), "");
        assert!(DocumentEdited::Marked.note().contains("net-zero key edit"));
        assert!(DocumentEdited::NotMarked
            .note()
            .contains("may keep the old content"));
        assert_eq!(DocumentEdited::NotADocument.structured(), None);
        assert_eq!(DocumentEdited::Marked.structured(), Some(true));
        assert_eq!(DocumentEdited::NotMarked.structured(), Some(false));
    }
}
