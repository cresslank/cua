// Pure visibility/identity policy shared by the GNOME extension and Node tests.
// Workspace indices are deliberately absent: visibility comes from Mutter's
// actual painting decision, which already accounts for sticky windows, dynamic
// workspace topology, monitor placement, and workspaces-only-on-primary.
export function targetIsPainted({ actorVisible, minimized, shellShowing }) {
    return Boolean(actorVisible) && !Boolean(minimized) && Boolean(shellShowing);
}

export function targetTokenMatches(epoch, targetId) {
    return typeof epoch === 'string'
        && epoch.length > 0
        && typeof targetId === 'string'
        && targetId.startsWith(`${epoch}:`);
}

export function captureAreaIsSafe({displayWidth, displayHeight, stageWidth, stageHeight}) {
    return [displayWidth, displayHeight, stageWidth, stageHeight]
        .every(value => Number.isFinite(value) && value >= 1);
}

export function captureContextIsSafe({overviewVisible, sessionLocked}) {
    return !Boolean(overviewVisible) && !Boolean(sessionLocked);
}
