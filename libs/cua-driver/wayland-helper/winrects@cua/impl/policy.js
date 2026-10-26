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
