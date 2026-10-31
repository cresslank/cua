// Mutter's default 'Super' binding is the left overlay key; an explicit
// Super_L is equivalent. Disabled/remapped/right-side bindings do not authorize
// the driver's fixed KEY_LEFTMETA dispatch.
export function overlayKeyUsesLeftSuper(binding) {
    return binding === 'Super' || binding === 'Super_L';
}

// Persistent switching is deliberately separate from temporary foreground input:
// overview, minimization and workspace changes are permitted only for this action.
export function persistentSwitchRefusal({sessionLocked, userSession, targetResolved, busy, modalChildPresent}) {
    if (sessionLocked !== false || userSession !== true) return 'session_locked';
    if (targetResolved !== true) return 'window_not_found';
    if (busy !== false || modalChildPresent !== false) return 'shell_not_ready';
    return null;
}

export function persistentSwitchFocused({sessionLocked, targetResolved, focusMatches, workspaceActive,
    minimized, overviewVisible, keyFocusInShellUi, modalCount}) {
    return sessionLocked === false && targetResolved === true && focusMatches === true
        && workspaceActive === true && minimized === false && overviewVisible === false
        && keyFocusInShellUi === false && modalCount === 0;
}

// Positive proof only; focus_window can remain an application while overview owns keys.
export function shellSearchCanRequestFocus({overviewVisible, sessionLocked, busy, entryAvailable, modalCount}) {
    return overviewVisible === true && sessionLocked === false && busy === false
        && entryAvailable === true && Number.isInteger(modalCount) && modalCount >= 0 && modalCount <= 1;
}

export function shellSearchHasFocus({overviewVisible, sessionLocked, searchEntryHasKeyFocus, keyFocusInShellUi, modalCount}) {
    return overviewVisible === true && sessionLocked === false
        && searchEntryHasKeyFocus === true && keyFocusInShellUi === true
        && Number.isInteger(modalCount) && modalCount >= 0 && modalCount <= 1;
}

// Pure visibility/identity policy shared by the GNOME extension and Node tests.
// showing_on_its_workspace() describes hypothetical visibility on that workspace,
// not membership of the active stage. Keep workspace membership explicit without
// depending on indices or the number of dynamic workspaces.
export function targetIsPainted({ actorVisible, minimized, shellShowing }) {
    return Boolean(actorVisible) && !Boolean(minimized) && Boolean(shellShowing);
}

export function targetIsOnActiveStage({sticky, workspaceActive, ...painting}) {
    return targetIsPainted(painting) && (Boolean(sticky) || Boolean(workspaceActive));
}

export function targetTokenMatches(epoch, targetId) {
    if (typeof epoch !== 'string' || epoch.length === 0 || typeof targetId !== 'string')
        return false;
    const prefix = `${epoch}:`;
    if (!targetId.startsWith(prefix))
        return false;
    const sequence = targetId.slice(prefix.length);
    return /^[1-9][0-9]*$/.test(sequence) && Number.isSafeInteger(Number(sequence));
}

export function rectanglesOverlap(a, b) {
    return [a?.x, a?.y, a?.width, a?.height, b?.x, b?.y, b?.width, b?.height]
        .every(Number.isFinite)
        && a.width > 0
        && a.height > 0
        && b.width > 0
        && b.height > 0
        && a.x < b.x + b.width
        && a.x + a.width > b.x
        && a.y < b.y + b.height
        && a.y + a.height > b.y;
}

export function captureDimensionsAreSafe(width, height) {
    return [width, height].every(value => Number.isFinite(value) && value >= 1);
}

export function captureAreaIsSafe({displayWidth, displayHeight, stageWidth, stageHeight}) {
    return captureDimensionsAreSafe(displayWidth, displayHeight)
        && captureDimensionsAreSafe(stageWidth, stageHeight);
}

export function captureRectangleIsSafe(rect, {displayWidth, displayHeight}) {
    return [rect?.x, rect?.y, rect?.width, rect?.height, displayWidth, displayHeight]
        .every(Number.isFinite)
        && rect.width >= 1
        && rect.height >= 1
        && rect.x >= 0
        && rect.y >= 0
        && rect.x + rect.width <= displayWidth
        && rect.y + rect.height <= displayHeight;
}

export function rectanglesEqual(a, b) {
    return Boolean(a) && Boolean(b)
        && a.x === b.x
        && a.y === b.y
        && a.width === b.width
        && a.height === b.height;
}

// Deterministic diagnostic precedence when several Shell guards are active.
// The existing boolean policy remains the authority to allow or refuse.
export function captureContextRefusalReason({overviewVisible, sessionLocked, modalCount, keyFocusInShellUi}) {
    if (overviewVisible)
        return 'overview_visible';
    if (sessionLocked)
        return 'session_locked';
    if (Number(modalCount || 0) > 0)
        return 'shell_modal_grab';
    if (keyFocusInShellUi)
        return 'shell_key_focus';
    return null;
}

export function captureContextIsSafe({overviewVisible, sessionLocked, shellInputGrabbed}) {
    return !Boolean(overviewVisible)
        && !Boolean(sessionLocked)
        && !Boolean(shellInputGrabbed);
}

export function shellInputIsGrabbed({modalCount, keyFocusInShellUi}) {
    return Number(modalCount || 0) > 0 || Boolean(keyFocusInShellUi);
}

export function foregroundTargetCanActivate({
    targetResolved,
    shellContextSafe,
    minimized,
    shellShowing,
    modalChildPresent,
}) {
    return Boolean(targetResolved)
        && Boolean(shellContextSafe)
        && !Boolean(minimized)
        && Boolean(shellShowing)
        && !Boolean(modalChildPresent);
}

export function foregroundTargetIsSafe({
    targetResolved,
    focusMatches,
    targetVisible,
    targetUnoccluded,
    modalChildPresent,
}) {
    return Boolean(targetResolved)
        && Boolean(focusMatches)
        && Boolean(targetVisible)
        && Boolean(targetUnoccluded)
        && !Boolean(modalChildPresent);
}
