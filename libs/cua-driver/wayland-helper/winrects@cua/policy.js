// Pure visibility/identity policy shared by the GNOME extension and Node tests.
// Workspace indices are deliberately absent: visibility comes from Mutter's
// actual painting decision, which already accounts for sticky windows, dynamic
// workspace topology, monitor placement, and workspaces-only-on-primary.
export function targetIsPainted({ actorVisible, minimized, shellShowing }) {
    return Boolean(actorVisible) && !Boolean(minimized) && Boolean(shellShowing);
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

export function captureAreaIsSafe({displayWidth, displayHeight, stageWidth, stageHeight}) {
    return [displayWidth, displayHeight, stageWidth, stageHeight]
        .every(value => Number.isFinite(value) && value >= 1);
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
