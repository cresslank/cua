import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Shell from 'gi://Shell';
import Meta from 'gi://Meta';
import St from 'gi://St';
import Clutter from 'gi://Clutter';
import Cairo from 'cairo';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import {
    captureAreaIsSafe,
    captureContextIsSafe,
    captureRectangleIsSafe,
    foregroundTargetCanActivate,
    foregroundTargetIsSafe,
    rectanglesOverlap,
    rectanglesEqual,
    shellInputIsGrabbed,
    targetIsPainted,
    targetTokenMatches,
} from './policy.js';

Gio._promisify(Shell.Screenshot.prototype, 'screenshot_area');

const HELPER_API_VERSION = 13;
const EXACT_TARGET_PROTOCOL_VERSION = 4;
const FOREGROUND_TIMEOUT_MS = 30_000;
const CURSOR_IDLE_TIMEOUT_US = 5 * 60 * 1_000_000;

const IFACE = `<node><interface name="org.cua.WinRects">
<method name="GetVersion"><arg type="u" direction="out" name="version"/></method>
<method name="GetCapabilities"><arg type="s" direction="out" name="json"/></method>
<method name="GetRects"><arg type="s" direction="out" name="json"/></method>
<method name="CaptureTarget"><arg type="s" direction="in" name="target"/><arg type="s" direction="out" name="capture_json"/></method>
<method name="BeginForeground"><arg type="s" direction="in" name="transaction"/><arg type="s" direction="in" name="target"/><arg type="s" direction="out" name="json"/></method>
<method name="QueryForeground"><arg type="s" direction="in" name="transaction"/><arg type="s" direction="out" name="json"/></method>
<method name="AbortForeground"><arg type="s" direction="in" name="transaction"/><arg type="s" direction="out" name="json"/></method>
<method name="ValidateForeground"><arg type="s" direction="in" name="transaction"/><arg type="s" direction="out" name="json"/></method>
<method name="EndForeground"><arg type="s" direction="in" name="transaction"/><arg type="s" direction="out" name="json"/></method>
<method name="CommitForeground"><arg type="s" direction="in" name="transaction"/><arg type="s" direction="out" name="json"/></method>
<method name="MoveCursorFor"><arg type="s" direction="in" name="owner"/><arg type="s" direction="in" name="target"/><arg type="i" direction="in" name="x"/><arg type="i" direction="in" name="y"/></method>
<method name="ClickPulseFor"><arg type="s" direction="in" name="owner"/><arg type="s" direction="in" name="target"/><arg type="i" direction="in" name="x"/><arg type="i" direction="in" name="y"/></method>
<method name="SetCursorColor"><arg type="s" direction="in" name="fill_color"/></method>
<method name="SetCursorState"><arg type="s" direction="in" name="action"/><arg type="s" direction="in" name="delivery"/><arg type="s" direction="in" name="target"/><arg type="b" direction="in" name="active"/></method>
<method name="SetSessionLabel"><arg type="s" direction="in" name="label"/></method>
<method name="SetCursorColorFor"><arg type="s" direction="in" name="owner"/><arg type="s" direction="in" name="fill_color"/></method>
<method name="SetCursorStateFor"><arg type="s" direction="in" name="owner"/><arg type="s" direction="in" name="action"/><arg type="s" direction="in" name="delivery"/><arg type="s" direction="in" name="target"/><arg type="b" direction="in" name="active"/></method>
<method name="SetSessionLabelFor"><arg type="s" direction="in" name="owner"/><arg type="s" direction="in" name="label"/></method>
<method name="HideCursorFor"><arg type="s" direction="in" name="owner"/></method>
<method name="RemoveCursor"><arg type="s" direction="in" name="owner"/></method>
<!-- v1 compatibility methods deliberately fail closed. An old driver must not
     render Shell-global chrome or activate/capture an unqualified target. -->
<method name="Capture"><arg type="s" direction="out" name="png_base64"/></method>
<method name="Activate"><arg type="u" direction="in" name="id"/><arg type="b" direction="out" name="activated"/></method>
<method name="MoveCursor"><arg type="i" direction="in" name="x"/><arg type="i" direction="in" name="y"/></method>
<method name="ClickPulse"><arg type="i" direction="in" name="x"/><arg type="i" direction="in" name="y"/></method>
<method name="HideCursor"></method>
</interface></node>`;

// GNOME cannot host the Rust renderer directly, so this Shell actor mirrors
// the embedded cua.default vector theme and semantic state vocabulary.
const CANVAS_SIZE = 128;
const DISPLAY_SIZE = 42;
const ACTOR_SIZE = 112;
const ACTOR_CENTER = ACTOR_SIZE / 2;
const SCALE = DISPLAY_SIZE / CANVAS_SIZE;
const FLOAT_DURATION = 4.0;
const GLOW_SURFACE_SCALE = 3;
const GLOW_PADDING = 24;
const BADGE_Y_OFFSET = 29;
const BADGE_HOLD_SECONDS = 2.0;
const BADGE_FADE_SECONDS = 0.4;
const BADGE_CHIP_SIZE = 18;
const ACTIONS = new Set([
  'idle',
  'observe',
  'click',
  'drag',
  'scroll',
  'text',
  'key',
  'navigate',
  'app',
  'transfer',
  'record',
  'system',
]);
const ONE_SHOT_ACTIONS = new Set(['click', 'key', 'navigate', 'app', 'system']);
const ACTION_DURATIONS = {
  idle: 4.0,
  click: 0.67,
  observe: 1.6,
  drag: 1.6,
  scroll: 1.6,
  text: 1.6,
  key: 1.6,
  navigate: 1.6,
  app: 1.6,
  transfer: 1.6,
  record: 1.6,
  system: 1.6,
};

function nowSeconds() {
  return GLib.get_monotonic_time() / 1_000_000;
}

function mixChannel(base, accent, weight) {
  return Math.round(base * (1 - weight) + accent * weight);
}

function badgeStyle(fillColor) {
  const start = fillColor.map((channel, index) => mixChannel([94, 151, 178][index], channel, 0.66));
  const end = fillColor.map((channel, index) => mixChannel([13, 27, 38][index], channel, 0.26));
  // The rim carries session identity now that the orb is gone, matching
  // paint_session_badge in the Rust renderer.
  const rim = fillColor.map((channel) => mixChannel(255, channel, 0.55));
  return [
    'spacing: 7px',
    'padding: 6px 10px',
    `background-gradient-start: rgba(${start[0]}, ${start[1]}, ${start[2]}, 0.93)`,
    `background-gradient-end: rgba(${end[0]}, ${end[1]}, ${end[2]}, 0.96)`,
    'background-gradient-direction: horizontal',
    `border: 1px solid rgba(${rim[0]}, ${rim[1]}, ${rim[2]}, 0.75)`,
    'border-radius: 14px',
    'box-shadow: 0 2px 9px rgba(0, 0, 0, 0.30)',
  ].join(';');
}

function easeInOut(value) {
  const t = Math.max(0, Math.min(1, value));
  return t * t * (3 - 2 * t);
}

function triangleWave(value) {
  const t = ((value % 1) + 1) % 1;
  return t < 0.5 ? t * 2 : (1 - t) * 2;
}

function setInk(cr, alpha = 1) {
  cr.setSourceRGBA(1, 1, 1, alpha);
}

function setPaper(cr, alpha = 1) {
  cr.setSourceRGBA(1, 1, 1, alpha);
}

function setFill(cr, color, alpha = 1) {
  cr.setSourceRGBA(color[0] / 255, color[1] / 255, color[2] / 255, alpha);
}

function glowPath(cr, width, alpha, glowColor) {
  if (!glowColor) return;
  const layers = 12;
  const outerExpansion = 13;
  const innerExpansion = 2.5;
  const maxOpacity = 0.17;
  let accumulatedOpacity = 0;
  for (let layer = 0; layer < layers; layer++) {
    const progress = (layer + 1) / layers;
    const expansion = outerExpansion + (innerExpansion - outerExpansion) * progress;
    const targetOpacity = maxOpacity * Math.max(0, Math.min(1, alpha)) * Math.pow(progress, 1.6);
    const layerOpacity =
      (targetOpacity - accumulatedOpacity) / Math.max(0.001, 1 - accumulatedOpacity);
    accumulatedOpacity = targetOpacity;
    cr.setLineWidth(width + expansion);
    cr.setLineCap(Cairo.LineCap.ROUND);
    cr.setLineJoin(Cairo.LineJoin.ROUND);
    setFill(cr, glowColor, layerOpacity);
    cr.strokePreserve();
  }
}

function strokePath(cr, width, alpha = 1, glowColor = null) {
  glowPath(cr, width, alpha, glowColor);
  cr.setLineWidth(glowColor ? width + 1.5 : width);
  cr.setLineCap(Cairo.LineCap.ROUND);
  cr.setLineJoin(Cairo.LineJoin.ROUND);
  setInk(cr, alpha);
  if (!glowColor) {
    cr.stroke();
    return;
  }
  cr.strokePreserve();
  cr.setLineWidth(Math.max(1.5, width - 1));
  setFill(cr, glowColor, alpha);
  cr.stroke();
}

function fillPath(cr, glowWidth, alpha = 1, glowColor = null) {
  glowPath(cr, glowWidth, alpha, glowColor);
  cr.setLineWidth(3);
  cr.setLineCap(Cairo.LineCap.ROUND);
  cr.setLineJoin(Cairo.LineJoin.ROUND);
  setInk(cr, alpha);
  cr.strokePreserve();
  setFill(cr, glowColor, alpha);
  cr.fill();
}

function linePath(cr, points, width = 4, alpha = 1, glowColor = null) {
  if (points.length === 0) return;
  cr.moveTo(points[0][0], points[0][1]);
  for (let i = 1; i < points.length; i++) cr.lineTo(points[i][0], points[i][1]);
  strokePath(cr, width, alpha, glowColor);
}

function roundedRect(cr, x, y, width, height, radius) {
  const r = Math.max(0, Math.min(radius, width / 2, height / 2));
  const k = 0.5522848;
  cr.moveTo(x + r, y);
  cr.lineTo(x + width - r, y);
  cr.curveTo(x + width - r + r * k, y, x + width, y + r - r * k, x + width, y + r);
  cr.lineTo(x + width, y + height - r);
  cr.curveTo(
    x + width,
    y + height - r + r * k,
    x + width - r + r * k,
    y + height,
    x + width - r,
    y + height
  );
  cr.lineTo(x + r, y + height);
  cr.curveTo(x + r - r * k, y + height, x, y + height - r + r * k, x, y + height - r);
  cr.lineTo(x, y + r);
  cr.curveTo(x, y + r - r * k, x + r - r * k, y, x + r, y);
  cr.closePath();
}

function traceCursorBody(cr) {
  cr.moveTo(55, 30);
  cr.curveTo(48, 28, 42, 33, 43, 41);
  cr.lineTo(64, 98);
  cr.curveTo(67, 106, 73, 106, 77, 99);
  cr.lineTo(86, 79);
  cr.curveTo(88, 75, 91, 72, 95, 70);
  cr.lineTo(108, 63);
  cr.curveTo(115, 59, 114, 53, 107, 50);
  cr.closePath();
}

function drawCursorGlowShape(cr, fillColor) {
  const layers = 36;
  const outerWidth = 44;
  const innerWidth = 7;
  const maxOpacity = 0.34;
  let accumulatedOpacity = 0;

  for (let layer = 0; layer < layers; layer++) {
    const progress = (layer + 1) / layers;
    const width = outerWidth + (innerWidth - outerWidth) * progress;
    const targetOpacity = maxOpacity * Math.pow(progress, 1.65);
    const layerOpacity =
      (targetOpacity - accumulatedOpacity) / Math.max(0.001, 1 - accumulatedOpacity);
    accumulatedOpacity = targetOpacity;
    if (layerOpacity <= 0) continue;

    traceCursorBody(cr);
    cr.setLineWidth(width);
    cr.setLineCap(Cairo.LineCap.ROUND);
    cr.setLineJoin(Cairo.LineJoin.ROUND);
    setFill(cr, fillColor, layerOpacity);
    cr.stroke();
  }
}

function createGlowSurface(fillColor) {
  const surface = new Cairo.ImageSurface(
    Cairo.Format.ARGB32,
    (CANVAS_SIZE + GLOW_PADDING * 2) * GLOW_SURFACE_SCALE,
    (CANVAS_SIZE + GLOW_PADDING * 2) * GLOW_SURFACE_SCALE
  );
  const cr = new Cairo.Context(surface);
  cr.scale(GLOW_SURFACE_SCALE, GLOW_SURFACE_SCALE);
  cr.translate(GLOW_PADDING, GLOW_PADDING);
  drawCursorGlowShape(cr, fillColor);
  cr.$dispose();
  return surface;
}

function sharedFloatMotion(progress) {
  const angle = progress * Math.PI * 2;
  return {
    dx: Math.sin(angle) * 5,
    dy: Math.cos(angle) * 6 - 5,
    rotation: Math.cos(angle) * ((2.5 * Math.PI) / 180),
    scale: 1,
  };
}

function cursorBodyMotion(progress, action) {
  let dx = 0;
  let dy = 0;
  let rotation = 0;
  let scale = 1;

  if (action === 'click') {
    if (progress < 0.35) scale = 1 - easeInOut(progress / 0.35) * 0.07;
    else if (progress < 0.6) scale = 0.93 + easeInOut((progress - 0.35) / 0.25) * 0.1;
    else scale = 1.03 - easeInOut((progress - 0.6) / 0.4) * 0.03;
  } else if (action === 'drag') {
    const held = easeInOut(triangleWave(progress));
    dx = held * 7;
    dy = held * 3;
  }

  return { dx, dy, rotation, scale };
}

function applyCursorBodyMotion(cr, motion) {
  cr.translate(64 + motion.dx, 64 + motion.dy);
  cr.rotate(motion.rotation);
  cr.scale(motion.scale, motion.scale);
  cr.translate(-64, -64);
}

function drawCursorGlow(cr, progress, action, glowSurface) {
  const motion = cursorBodyMotion(progress, action);
  cr.save();
  applyCursorBodyMotion(cr, motion);
  cr.translate(-GLOW_PADDING, -GLOW_PADDING);
  cr.scale(1 / GLOW_SURFACE_SCALE, 1 / GLOW_SURFACE_SCALE);
  cr.setSourceSurface(glowSurface, 0, 0);
  cr.paint();
  cr.restore();
}

function drawCursorBody(cr, progress, action, fillColor) {
  const motion = cursorBodyMotion(progress, action);
  cr.save();
  applyCursorBodyMotion(cr, motion);
  traceCursorBody(cr);
  setFill(cr, fillColor);
  cr.fillPreserve();
  cr.setLineWidth(5);
  cr.setLineCap(Cairo.LineCap.ROUND);
  cr.setLineJoin(Cairo.LineJoin.ROUND);
  setPaper(cr);
  cr.stroke();
  cr.restore();
}

function drawActionCue(cr, action, progress, fillColor) {
  const wave = triangleWave(progress);
  const strokeCue = (width, alpha = 1) => strokePath(cr, width, alpha, fillColor);
  const lineCue = (points, width = 4, alpha = 1) => linePath(cr, points, width, alpha, fillColor);
  cr.save();
  switch (action) {
    case 'observe': {
      const opacity = Math.min(1, progress * 7);
      cr.translate(8, -10);
      cr.moveTo(38, 28);
      cr.curveTo(27, 29, 20, 38, 20, 49);
      strokeCue(4, opacity);
      cr.moveTo(42, 19);
      cr.curveTo(23, 19, 11, 33, 11, 51);
      strokeCue(4, opacity);
      break;
    }
    case 'click': {
      const cueProgress = Math.max(0, Math.min(1, progress / 0.65));
      const opacity = Math.max(
        0,
        Math.min(1, Math.min(1 - Math.pow(cueProgress, 1.35), cueProgress * 5))
      );
      const cueScale = 1.1 + easeInOut(cueProgress) * 0.4;
      cr.translate(35, 28);
      cr.scale(cueScale, cueScale);
      cr.translate(-25, -25);
      lineCue(
        [
          [35, 20],
          [34, 11],
        ],
        4,
        opacity
      );
      lineCue(
        [
          [27, 25],
          [19, 19],
        ],
        4,
        opacity
      );
      lineCue(
        [
          [25, 34],
          [15, 34],
        ],
        4,
        opacity
      );
      break;
    }
    case 'drag': {
      const offset = easeInOut(wave) * 7;
      cr.translate(offset, offset * 0.43);
      lineCue(
        [
          [28, 38],
          [16, 35],
        ],
        4,
        0.2 + wave * 0.8
      );
      lineCue(
        [
          [26, 48],
          [12, 45],
        ],
        4,
        0.2 + wave * 0.8
      );
      break;
    }
    case 'scroll': {
      cr.translate(-5, 4 - wave * 8);
      lineCue(
        [
          [23, 31],
          [31, 22],
          [39, 31],
        ],
        4,
        0.42 + wave * 0.58
      );
      lineCue(
        [
          [23, 49],
          [31, 58],
          [39, 49],
        ],
        4,
        0.42 + wave * 0.58
      );
      break;
    }
    case 'text': {
      const opacity = progress < 0.34 || progress > 0.64 ? 1 : 0.18;
      cr.translate(-4, 0);
      lineCue(
        [
          [31, 22],
          [31, 58],
        ],
        4,
        opacity
      );
      lineCue(
        [
          [24, 22],
          [38, 22],
        ],
        4,
        opacity
      );
      lineCue(
        [
          [24, 58],
          [38, 58],
        ],
        4,
        opacity
      );
      break;
    }
    case 'key': {
      const bounce = Math.sin(progress * Math.PI * 2) * (1 - progress) * 3;
      cr.translate(-9, bounce);
      roundedRect(cr, 14, 25, 28, 28, 6);
      strokeCue(3.5);
      lineCue(
        [
          [23, 32],
          [23, 46],
          [23, 39],
          [33, 32],
          [24, 39],
          [34, 46],
        ],
        3.5
      );
      break;
    }
    case 'navigate': {
      cr.translate(-10 + easeInOut(progress) * 9, 0);
      lineCue(
        [
          [15, 29],
          [25, 40],
          [15, 51],
        ],
        4,
        0.2 + wave * 0.8
      );
      lineCue(
        [
          [29, 29],
          [39, 40],
          [29, 51],
        ],
        4,
        0.2 + wave * 0.8
      );
      break;
    }
    case 'app': {
      const s = 0.2 + easeInOut(Math.min(1, progress * 2)) * 0.8;
      cr.translate(21, 39);
      cr.scale(s, s);
      cr.translate(-26, -39);
      for (const [x, y] of [
        [13, 26],
        [29, 26],
        [13, 42],
        [29, 42],
      ]) {
        roundedRect(cr, x, y, 10, 10, 2);
        strokeCue(3.5);
      }
      break;
    }
    case 'transfer':
      cr.translate(-9, 6 - wave * 12);
      lineCue(
        [
          [22, 50],
          [22, 20],
          [14, 28],
          [22, 20],
          [30, 28],
        ],
        4,
        0.38 + wave * 0.62
      );
      lineCue(
        [
          [37, 28],
          [37, 58],
          [29, 50],
          [37, 58],
          [45, 50],
        ],
        4,
        0.38 + wave * 0.62
      );
      break;
    case 'record':
      cr.translate(-12, 0);
      cr.arc(29, 39, 17, 0, Math.PI * 2);
      strokeCue(4);
      cr.arc(29, 39, 3.6 + wave * 2.1, 0, Math.PI * 2);
      fillPath(cr, (3.6 + wave * 2.1) * 1.25, 0.42 + wave * 0.58, fillColor);
      break;
    case 'system': {
      cr.translate(15, 39);
      cr.rotate(((easeInOut(progress) * 68 - 18) * Math.PI) / 180);
      cr.translate(-29, -39);
      for (const radius of [12, 4]) {
        cr.arc(29, 39, radius, 0, Math.PI * 2);
        strokeCue(3.5);
      }
      for (const points of [
        [
          [29, 20],
          [29, 25],
        ],
        [
          [29, 53],
          [29, 58],
        ],
        [
          [10, 39],
          [15, 39],
        ],
        [
          [43, 39],
          [48, 39],
        ],
        [
          [16, 26],
          [20, 30],
        ],
        [
          [38, 48],
          [42, 52],
        ],
        [
          [16, 52],
          [20, 48],
        ],
        [
          [38, 30],
          [42, 26],
        ],
      ])
        lineCue(points, 3.5);
      break;
    }
    default:
      break;
  }
  cr.restore();
}

function drawBadgeChip(cr, glyph, filled, fillColor) {
  roundedRect(cr, 0.5, 0.5, 17, 17, 5);
  if (filled) {
    cr.setSourceRGBA(fillColor[0] / 255, fillColor[1] / 255, fillColor[2] / 255, 0.86);
    cr.fillPreserve();
  } else {
    cr.setSourceRGBA(1, 1, 1, 0.09);
    cr.fillPreserve();
  }
  cr.setSourceRGBA(1, 1, 1, filled ? 0.72 : 0.42);
  cr.setLineWidth(1);
  cr.stroke();
  cr.setSourceRGBA(1, 1, 1, 0.95);
  cr.setLineWidth(1.35);
  cr.setLineCap(Cairo.LineCap.ROUND);
  cr.setLineJoin(Cairo.LineJoin.ROUND);

  const stroke = () => cr.stroke();
  const line = (points) => {
    cr.moveTo(points[0][0], points[0][1]);
    for (const [x, y] of points.slice(1)) cr.lineTo(x, y);
    stroke();
  };
  switch (glyph) {
    case 'background':
      roundedRect(cr, 4, 4, 7, 7, 1.8);
      stroke();
      roundedRect(cr, 6.4, 6.4, 7, 7, 1.8);
      stroke();
      break;
    case 'foreground':
      roundedRect(cr, 4, 5, 10, 9, 1.8);
      stroke();
      line([
        [5, 7.6],
        [13, 7.6],
      ]);
      break;
    case 'ax':
      line([
        [9, 5],
        [9, 9],
        [5, 13],
      ]);
      line([
        [9, 9],
        [13, 13],
      ]);
      for (const [x, y] of [
        [9, 5],
        [5, 13],
        [13, 13],
      ]) {
        cr.arc(x, y, 1.35, 0, Math.PI * 2);
        cr.fill();
      }
      break;
    case 'pixel':
      line([
        [4, 7],
        [4, 4],
        [7, 4],
      ]);
      line([
        [11, 4],
        [14, 4],
        [14, 7],
      ]);
      line([
        [14, 11],
        [14, 14],
        [11, 14],
      ]);
      line([
        [7, 14],
        [4, 14],
        [4, 11],
      ]);
      break;
    case 'browser':
      cr.arc(9, 9, 5, 0, Math.PI * 2);
      stroke();
      line([
        [4, 9],
        [14, 9],
      ]);
      line([
        [9, 4],
        [7, 9],
        [9, 14],
        [11, 9],
        [9, 4],
      ]);
      break;
    case 'desktop':
      roundedRect(cr, 4, 4, 10, 7.6, 1.3);
      stroke();
      line([
        [9, 11.6],
        [9, 14],
        [6, 14],
        [12, 14],
      ]);
      break;
    default:
      break;
  }
}

export default class WinRectsExtension extends Extension {
    enable() {
        this._epoch = GLib.uuid_string_random();
        this._cursors = new Map();
        this._signals = [];
        this._foreground = null;
        this._impl = Gio.DBusExportedObject.wrapJSObject(IFACE, this);
        this._impl.export(Gio.DBus.session, '/org/cua/WinRects');
        this._nameId = Gio.bus_own_name(Gio.BusType.SESSION, 'org.cua.WinRects',
            Gio.BusNameOwnerFlags.REPLACE, null, null, null);
        this._nameOwnerSignalId = Gio.DBus.session.signal_subscribe(
            'org.freedesktop.DBus',
            'org.freedesktop.DBus',
            'NameOwnerChanged',
            '/org/freedesktop/DBus',
            null,
            Gio.DBusSignalFlags.NONE,
            (_connection, _sender, _path, _interface, _signal, parameters) => {
                const [name, oldOwner, newOwner] = parameters.deep_unpack();
                if (name.startsWith(':') && oldOwner && !newOwner)
                    this._removeCursorsForConnection(name);
            }
        );

        const update = () => this._scheduleCursorVisibilityUpdate();
        this._connect(global.workspace_manager, 'active-workspace-changed', update);
        this._connect(global.display, 'restacked', update);
        this._connect(global.display, 'window-created', update);
        this._connect(global.display, 'window-entered-monitor', update);
        this._connect(global.display, 'window-left-monitor', update);
        this._connect(Main.layoutManager, 'monitors-changed', update);
        this._cursorReaperId = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, 60, () => {
            const cutoff = GLib.get_monotonic_time() - CURSOR_IDLE_TIMEOUT_US;
            for (const [key, record] of this._cursors) {
                if (record.lastUsedAt < cutoff)
                    this._removeCursor(key);
            }
            return GLib.SOURCE_CONTINUE;
        });
    }

    disable() {
        if (this._foreground)
            this._finishForegroundAsync(this._foreground.transaction, 'extension-disabled', null);
        for (const [object, id] of this._signals) {
            try { object.disconnect(id); } catch (_error) {}
        }
        this._signals = [];
        for (const owner of [...this._cursors.keys()])
            this._removeCursor(owner);
        this._cursors.clear();
        if (this._cursorReaperId) {
            try { GLib.source_remove(this._cursorReaperId); } catch (_error) {}
            this._cursorReaperId = 0;
        }
        if (this._nameOwnerSignalId) {
            Gio.DBus.session.signal_unsubscribe(this._nameOwnerSignalId);
            this._nameOwnerSignalId = 0;
        }
        if (this._impl) { this._impl.unexport(); this._impl = null; }
        if (this._nameId) { Gio.bus_unown_name(this._nameId); this._nameId = 0; }
    }

    _connect(object, signal, callback) {
        try {
            this._signals.push([object, object.connect(signal, callback)]);
        } catch (_error) {
            // Signals vary across supported Shell releases. Cursor visibility
            // is also revalidated on every command, so a missing optional
            // signal cannot make an action target a different window.
        }
    }

    _createCursorRecord(owner) {
        const record = {
            connectionOwner: owner.connectionOwner,
            targetId: null,
            requestedVisible: false,
            lastUsedAt: GLib.get_monotonic_time(),
            targetSignals: [],
            targetWindow: null,
            fillColor: [94, 192, 232],
            fillColorCss: '#5ec0e8',
            action: 'idle',
            delivery: '',
            target: '',
            active: false,
            sessionLabel: '',
            actionStarted: nowSeconds(),
            endingAt: null,
            cursorX: null,
            cursorY: null,
            badgeRevealedAt: null,
            modifierFadeAt: null,
            frameId: 0,
            glowSurface: null,
        };
        record.glowSurface = createGlowSurface(record.fillColor);
        record.actor = new St.DrawingArea({
            width: ACTOR_SIZE, height: ACTOR_SIZE, visible: false,
            reactive: false, can_focus: false,
        });
        record.actor.connect('repaint', area => {
            const cr = area.get_context();
            const duration = ACTION_DURATIONS[record.action] ?? 1.6;
            const elapsed = nowSeconds() - record.actionStarted;
            const progress = (elapsed % duration) / duration;
            const floatProgress = (elapsed % FLOAT_DURATION) / FLOAT_DURATION;
            cr.save();
            cr.translate(ACTOR_CENTER, ACTOR_CENTER);
            cr.scale(SCALE, SCALE);
            cr.translate(-CANVAS_SIZE / 2, -CANVAS_SIZE / 2);
            applyCursorBodyMotion(cr, sharedFloatMotion(floatProgress));
            drawCursorGlow(cr, progress, record.action, record.glowSurface);
            drawActionCue(cr, record.action, progress, record.fillColor);
            drawCursorBody(cr, progress, record.action, record.fillColor);
            cr.restore();
            cr.$dispose();
        });
        record.actor.set_pivot_point(0.5, 0.5);
        Main.layoutManager.addTopChrome(record.actor);
        record.badgeLabel = new St.Label({
            text: '', visible: false, y_align: Clutter.ActorAlign.CENTER,
            style: 'font-size: 11px; font-weight: 600; color: white;',
        });
        record.deliveryChip = new St.DrawingArea({width: BADGE_CHIP_SIZE, height: BADGE_CHIP_SIZE, visible: false});
        record.deliveryChip.connect('repaint', area => {
            const cr = area.get_context(); drawBadgeChip(cr, record.delivery, true, record.fillColor); cr.$dispose();
        });
        record.targetChip = new St.DrawingArea({width: BADGE_CHIP_SIZE, height: BADGE_CHIP_SIZE, visible: false});
        record.targetChip.connect('repaint', area => {
            const cr = area.get_context(); drawBadgeChip(cr, record.target, false, record.fillColor); cr.$dispose();
        });
        record.badge = new St.BoxLayout({
            visible: false, reactive: false, can_focus: false, style: badgeStyle(record.fillColor),
        });
        record.badge.add_child(record.badgeLabel);
        record.badge.add_child(record.deliveryChip);
        record.badge.add_child(record.targetChip);
        Main.layoutManager.addTopChrome(record.badge);
        record.frameId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 33, () => {
            if (!record.actor) return GLib.SOURCE_REMOVE;
            const now = nowSeconds();
            if (record.endingAt !== null && now >= record.endingAt)
                this._setRecordCursorState(record, 'idle', '', '');
            if (ONE_SHOT_ACTIONS.has(record.action) && now - record.actionStarted >= ACTION_DURATIONS[record.action])
                this._setRecordCursorState(record, 'idle', '', '');
            if (record.actor.visible) record.actor.queue_repaint();
            let labelAlpha = record.badgeRevealedAt === null ? 0 : 1;
            if (record.badgeRevealedAt !== null) {
                const elapsed = now - record.badgeRevealedAt;
                if (elapsed > BADGE_HOLD_SECONDS && elapsed < BADGE_HOLD_SECONDS + BADGE_FADE_SECONDS)
                    labelAlpha = 1 - easeInOut((elapsed - BADGE_HOLD_SECONDS) / BADGE_FADE_SECONDS);
                else if (elapsed >= BADGE_HOLD_SECONDS + BADGE_FADE_SECONDS) {
                    labelAlpha = 0; record.badgeRevealedAt = null;
                }
            }
            let chipAlpha = record.delivery || record.target ? 1 : 0;
            if (record.modifierFadeAt !== null) {
                const fade = (now - record.modifierFadeAt) / BADGE_FADE_SECONDS;
                chipAlpha = 1 - easeInOut(fade);
                if (fade >= 1) {
                    chipAlpha = 0; record.modifierFadeAt = null; record.delivery = ''; record.target = '';
                    this._updateModifierChips(record);
                }
            }
            record.badgeLabel.opacity = Math.round(255 * labelAlpha);
            if (labelAlpha <= 0.001) record.badgeLabel.hide();
            record.deliveryChip.opacity = Math.round(255 * chipAlpha);
            record.targetChip.opacity = Math.round(255 * chipAlpha);
            // Exact-target gating is load-bearing: animation may never reveal a global cursor.
            const target = record.targetId ? this._resolveTarget(record.targetId) : null;
            const targetVisible = Boolean(record.requestedVisible && target && this._isTargetVisible(target));
            if (targetVisible && (labelAlpha > 0.001 || chipAlpha > 0.001)) record.badge.show();
            else record.badge.hide();
            return GLib.SOURCE_CONTINUE;
        });
        return record;
    }

    _positionBadge(record, x, y, duration) {
        const [, naturalWidth] = record.badge.get_preferred_width(-1);
        const badgeX = Math.round(x - naturalWidth / 2);
        const badgeY = Math.round(y + BADGE_Y_OFFSET);
        if (duration > 0)
            record.badge.ease({x: badgeX, y: badgeY, duration, mode: Clutter.AnimationMode.EASE_OUT_CUBIC});
        else record.badge.set_position(badgeX, badgeY);
    }

    _updateModifierChips(record) {
        record.deliveryChip.visible = record.delivery.length > 0;
        record.targetChip.visible = record.target.length > 0;
        record.deliveryChip.queue_repaint();
        record.targetChip.queue_repaint();
    }

    _setRecordCursorState(record, action, delivery, target) {
        record.action = ACTIONS.has(action) ? action : 'idle';
        const nextDelivery = delivery ?? '';
        const nextTarget = target ?? '';
        if (nextDelivery || nextTarget) {
            record.delivery = nextDelivery; record.target = nextTarget; record.modifierFadeAt = null;
        } else if ((record.delivery || record.target) && record.modifierFadeAt === null) {
            record.modifierFadeAt = nowSeconds();
        }
        this._updateModifierChips(record);
        record.actionStarted = nowSeconds(); record.endingAt = null;
        record.actor.queue_repaint();
    }

    _targetId(window) {
        return `${this._epoch}:${window.get_stable_sequence()}`;
    }

    _resolveTarget(targetId) {
        if (!targetTokenMatches(this._epoch, targetId))
            return null;
        const sequence = Number.parseInt(targetId.slice(this._epoch.length + 1), 10);
        if (!Number.isSafeInteger(sequence) || sequence <= 0)
            return null;
        return global.get_window_actors()
            .map(actor => actor.meta_window)
            .find(window => window?.get_stable_sequence() === sequence) ?? null;
    }

    _actorFor(window) {
        return global.get_window_actors()
            .find(actor => actor.meta_window === window) ?? null;
    }

    _keyFocusInShellUi() {
        let actor = global.stage.get_key_focus();
        while (actor) {
            if (actor === Main.uiGroup)
                return true;
            try {
                actor = actor.get_parent();
            } catch (_error) {
                return false;
            }
        }
        return false;
    }

    _captureContextIsSafe() {
        return captureContextIsSafe({
            overviewVisible: Main.overview?.visible,
            sessionLocked: Main.sessionMode?.isLocked,
            shellInputGrabbed: shellInputIsGrabbed({
                modalCount: Main.modalCount,
                keyFocusInShellUi: this._keyFocusInShellUi(),
            }),
        });
    }

    _windowShowing(window) {
        try {
            return window.showing_on_its_workspace();
        } catch (_error) {
            const workspace = window.get_workspace();
            return window.is_on_all_workspaces()
                || workspace === global.workspace_manager.get_active_workspace();
        }
    }

    _isTargetVisible(window) {
        if (!window || !this._captureContextIsSafe())
            return false;
        const actor = this._actorFor(window);
        return targetIsPainted({
            actorVisible: Boolean(actor?.visible),
            minimized: window.minimized,
            shellShowing: this._windowShowing(window),
        });
    }

    _canActivateTarget(window) {
        return foregroundTargetCanActivate({
            targetResolved: Boolean(window),
            shellContextSafe: this._captureContextIsSafe(),
            minimized: window?.minimized,
            shellShowing: window ? this._windowShowing(window) : false,
            modalChildPresent: window ? Boolean(this._visibleModalChild(window)) : true,
        });
    }

    _isTargetUnoccluded(window) {
        if (!this._isTargetVisible(window))
            return false;
        const windows = global.display.sort_windows_by_stacking(
            global.get_window_actors().map(actor => actor.meta_window).filter(Boolean)
        );
        const targetIndex = windows.indexOf(window);
        if (targetIndex < 0)
            return false;
        const targetRect = window.get_frame_rect();
        return !windows.slice(targetIndex + 1).some(candidate =>
            candidate !== window
            && !candidate.minimized
            && this._windowShowing(candidate)
            && rectanglesOverlap(targetRect, candidate.get_frame_rect())
        );
    }

    _visibleModalChild(target) {
        for (const actor of global.get_window_actors()) {
            const window = actor.meta_window;
            if (
                !window
                || window === target
                || window.minimized
                || !this._windowShowing(window)
            )
                continue;
            let parent = null;
            let attached = false;
            let modal = false;
            try { parent = window.get_transient_for(); } catch (_error) {}
            try { attached = Boolean(window.is_attached_dialog()); } catch (_error) {}
            try { modal = window.get_window_type() === Meta.WindowType.MODAL_DIALOG; } catch (_error) {}
            if (parent === target && (attached || modal))
                return window;
        }
        return null;
    }

    _windowAppId(window) {
        for (const getter of ['get_gtk_application_id', 'get_sandboxed_app_id', 'get_wm_class']) {
            try {
                const value = window[getter]?.call(window);
                if (value)
                    return value;
            } catch (_error) {}
        }
        return '';
    }

    GetVersion() { return HELPER_API_VERSION; }

    GetCapabilities() {
        return JSON.stringify({
            protocol_version: EXACT_TARGET_PROTOCOL_VERSION,
            epoch: this._epoch,
            capabilities: [
                'exact-target-v2',
                'workspace-metadata',
                'target-stage-capture',
                'atomic-target-capture-v1',
                'keyed-target-cursors',
                'connection-owned-cursors-v1',
                'foreground-transaction',
                'foreground-reconcile-v1',
                'foreground-revalidate-v1',
                'transient-parent-v1',
                'unoccluded-target-v1',
                'trusted-cursor-overlay-v1',
                'exact-target-activation-v1',
                'shell-grab-classification-v1',
            ],
        });
    }

    GetRects() {
        const actors = global.get_window_actors();
        const actorByWindow = new Map();
        for (const actor of actors) {
            if (actor.meta_window)
                actorByWindow.set(actor.meta_window, actor);
        }
        const windows = global.display.sort_windows_by_stacking([...actorByWindow.keys()]);
        const focusedWindow = global.display.focus_window;
        const out = [];
        for (let stacking = 0; stacking < windows.length; stacking++) {
            const w = windows[stacking];
            const actor = actorByWindow.get(w);
            const r = w.get_frame_rect();
            let buffer = r;
            try {
                buffer = w.get_buffer_rect();
            } catch (_error) {
                // Older Shell releases may not expose the buffer rectangle.
            }
            const minimized = Boolean(w.minimized);
            const workspace = w.get_workspace();
            const activeWorkspace = global.workspace_manager.get_active_workspace();
            let sticky = false;
            try { sticky = Boolean(w.is_on_all_workspaces()); } catch (_error) {}
            let workspaceIndex = -1;
            try { workspaceIndex = workspace?.index() ?? -1; } catch (_error) {}
            const captureCurrent = this._isTargetVisible(w) && this._isTargetUnoccluded(w);
            let transientFor = null;
            try { transientFor = w.get_transient_for(); } catch (_error) {}
            if (transientFor && !actorByWindow.has(transientFor))
                transientFor = null;
            let attachedDialog = false;
            try { attachedDialog = Boolean(w.is_attached_dialog()); } catch (_error) {}
            let windowType = null;
            try { windowType = w.get_window_type(); } catch (_error) {}
            const isModal = windowType === Meta.WindowType.MODAL_DIALOG;
            out.push({
                id: w.get_stable_sequence(),
                target_id: this._targetId(w),
                helper_epoch: this._epoch,
                protocol_version: EXACT_TARGET_PROTOCOL_VERSION,
                pid: w.get_pid(),
                app_id: this._windowAppId(w),
                title: w.get_title() ?? '',
                x: r.x,
                y: r.y,
                w: r.width,
                h: r.height,
                buffer_x: buffer.x,
                buffer_y: buffer.y,
                focused: focusedWindow === w,
                minimized,
                visible: captureCurrent,
                capture_current: captureCurrent,
                workspace_index: workspaceIndex,
                workspace_active: sticky || workspace === activeWorkspace,
                workspace_count: global.workspace_manager.n_workspaces,
                sticky,
                monitor: w.get_monitor(),
                monitor_primary: w.get_monitor() === Main.layoutManager.primaryIndex,
                stacking,
                transient_for_target_id: transientFor ? this._targetId(transientFor) : null,
                is_attached_dialog: attachedDialog,
                is_modal: isModal,
                window_type: windowType,
            });
        }
        return JSON.stringify(out);
    }

    async CaptureTargetAsync([targetId], invocation) {
        try {
            const target = this._resolveTarget(targetId);
            if (!target)
                throw new Error('stale_target: target belongs to another helper incarnation or no longer exists');
            if (!this._isTargetVisible(target))
                throw new Error('capture_foreground_required: target is not currently painted on the GNOME stage');
            if (!this._isTargetUnoccluded(target))
                throw new Error('capture_occluded: target is overlapped by a higher-stacked window');
            const [displayWidth, displayHeight] = global.display.get_size();
            const [stageWidth, stageHeight] = global.stage.get_size();
            if (!captureAreaIsSafe({displayWidth, displayHeight, stageWidth, stageHeight}))
                throw new Error(
                    'capture_not_ready: refusing GNOME screenshot with invalid '
                    + `display ${displayWidth}x${displayHeight} or stage ${stageWidth}x${stageHeight}`
                );
            const width = Math.floor(displayWidth);
            const height = Math.floor(displayHeight);
            const frame = target.get_frame_rect();
            const captureRect = {
                x: Math.floor(frame.x),
                y: Math.floor(frame.y),
                width: Math.floor(frame.width),
                height: Math.floor(frame.height),
            };
            if (!captureRectangleIsSafe(captureRect, {displayWidth: width, displayHeight: height}))
                throw new Error('capture_geometry_invalid: target rectangle is outside the captured stage');
            const shooter = new Shell.Screenshot();
            const stream = Gio.MemoryOutputStream.new_resizable();
            // Never call Shell.Screenshot.screenshot() here. GNOME 50 can pass
            // an implicit 0x0 stage view into Cogl immediately after a window
            // activation. Capture only the exact positive target rectangle so
            // this target API can never emit broad stage pixels to its caller.
            await shooter.screenshot_area(
                captureRect.x,
                captureRect.y,
                captureRect.width,
                captureRect.height,
                stream
            );
            if (this._resolveTarget(targetId) !== target)
                throw new Error('target_changed_during_capture');
            const currentFrame = target.get_frame_rect();
            const currentRect = {
                x: Math.floor(currentFrame.x),
                y: Math.floor(currentFrame.y),
                width: Math.floor(currentFrame.width),
                height: Math.floor(currentFrame.height),
            };
            if (!rectanglesEqual(captureRect, currentRect))
                throw new Error('target_geometry_changed_during_capture');
            if (!this._isTargetVisible(target))
                throw new Error('capture_context_changed');
            if (this._visibleModalChild(target))
                throw new Error('child_modal_appeared_during_capture');
            if (!this._isTargetUnoccluded(target))
                throw new Error('target_occluded_during_capture');
            stream.close(null);
            const encoded = GLib.base64_encode(stream.steal_as_bytes().get_data());
            invocation.return_value(new GLib.Variant('(s)', [JSON.stringify({
                protocol_version: EXACT_TARGET_PROTOCOL_VERSION,
                target: targetId,
                rect: captureRect,
                logical_size: {width, height},
                png_base64: encoded,
            })]));
        } catch (error) {
            invocation.return_dbus_error('org.cua.WinRects.CaptureFailed', String(error));
        }
    }

    BeginForegroundAsync([transaction, targetId], invocation) {
        if (typeof transaction !== 'string' || !/^cua-fg-[A-Za-z0-9._:-]{8,160}$/.test(transaction)) {
            invocation.return_dbus_error(
                'org.cua.WinRects.InvalidTransaction',
                'invalid_transaction: caller must allocate a bounded transaction ID before BeginForeground'
            );
            return;
        }
        if (this._foreground) {
            if (this._foreground.transaction === transaction && this._foreground.targetId === targetId) {
                invocation.return_value(new GLib.Variant('(s)', [
                    JSON.stringify(this._foregroundStatus(this._foreground)),
                ]));
                return;
            }
            invocation.return_dbus_error(
                'org.cua.WinRects.InputBusy',
                'input_busy: another GNOME foreground transaction is active'
            );
            return;
        }
        const target = this._resolveTarget(targetId);
        if (!target) {
            invocation.return_dbus_error(
                'org.cua.WinRects.StaleTarget',
                'stale_target: target belongs to another helper incarnation or no longer exists'
            );
            return;
        }
        if (!this._canActivateTarget(target)) {
            invocation.return_dbus_error(
                'org.cua.WinRects.TargetNotActivatable',
                'target_not_activatable: target is minimized, off-workspace, modal-blocked, or Shell context is unsafe'
            );
            return;
        }
        const priorWindow = global.display.focus_window;
        const priorWorkspace = global.workspace_manager.get_active_workspace();
        this._foreground = {
            transaction,
            target,
            targetId,
            priorWindow,
            priorWorkspace,
            timeoutId: 0,
            state: priorWindow === target ? 'active' : 'activating',
            activationRequired: priorWindow !== target,
            activated: priorWindow === target,
        };
        if (priorWindow !== target)
            target.activate(global.get_current_time());
        GLib.timeout_add(GLib.PRIORITY_DEFAULT, 100, () => {
            const foreground = this._foreground;
            if (!foreground || foreground.transaction !== transaction) {
                invocation.return_value(new GLib.Variant('(s)', [JSON.stringify({
                    terminal: true,
                    state: 'terminal',
                    activated: false,
                    reason: 'stale_transaction',
                })]));
                return GLib.SOURCE_REMOVE;
            }
            if (global.display.focus_window === target) {
                foreground.state = 'active';
                foreground.activated = true;
            } else {
                // Keep the transaction queryable. The caller may have lost this
                // reply while Mutter completes activation later; AbortForeground
                // reconciles by the caller-supplied ID before the raw lease ends.
                foreground.state = 'reconciling';
            }
            if (foreground.activated && this._visibleModalChild(target)) {
                foreground.state = 'reconciling';
                invocation.return_value(new GLib.Variant('(s)', [JSON.stringify({
                    activated: false,
                    terminal: false,
                    state: 'reconciling',
                    transaction,
                    reason: 'child_modal_present',
                })]));
                return GLib.SOURCE_REMOVE;
            }
            if (foreground.activated && !this._isTargetUnoccluded(target)) {
                foreground.state = 'reconciling';
                invocation.return_value(new GLib.Variant('(s)', [JSON.stringify({
                    activated: false,
                    terminal: false,
                    state: 'reconciling',
                    transaction,
                    reason: 'target_occluded',
                })]));
                return GLib.SOURCE_REMOVE;
            }
            foreground.timeoutId = GLib.timeout_add(
                GLib.PRIORITY_DEFAULT,
                FOREGROUND_TIMEOUT_MS,
                () => {
                    this._finishForegroundAsync(transaction, 'deadline', null);
                    return GLib.SOURCE_REMOVE;
                }
            );
            invocation.return_value(new GLib.Variant('(s)', [
                JSON.stringify(this._foregroundStatus(foreground)),
            ]));
            return GLib.SOURCE_REMOVE;
        });
    }

    _foregroundStatus(foreground) {
        if (foreground.state !== 'restoring' && global.display.focus_window === foreground.target) {
            foreground.state = 'active';
            foreground.activated = true;
        }
        let priorWorkspaceIndex = -1;
        try { priorWorkspaceIndex = foreground.priorWorkspace?.index() ?? -1; } catch (_error) {}
        return {
            terminal: false,
            state: foreground.state,
            activated: foreground.activated,
            activation_required: foreground.activationRequired,
            transaction: foreground.transaction,
            target: foreground.targetId,
            prior_workspace_index: priorWorkspaceIndex,
            prior_window: foreground.priorWindow ? this._targetId(foreground.priorWindow) : null,
        };
    }

    QueryForeground(transaction) {
        const foreground = this._foreground;
        if (!foreground || (transaction && foreground.transaction !== transaction))
            return JSON.stringify({terminal: true, state: 'terminal', reason: 'stale_transaction'});
        return JSON.stringify(this._foregroundStatus(foreground));
    }

    ValidateForeground(transaction) {
        const foreground = this._foreground;
        if (!foreground || foreground.transaction !== transaction)
            return JSON.stringify({valid: false, reason: 'stale_transaction'});
        const currentTarget = this._resolveTarget(foreground.targetId);
        const modalChildPresent = Boolean(this._visibleModalChild(foreground.target));
        const targetUnoccluded = this._isTargetUnoccluded(foreground.target);
        const targetSafe = foregroundTargetIsSafe({
            targetResolved: currentTarget === foreground.target,
            focusMatches: global.display.focus_window === foreground.target,
            targetVisible: this._isTargetVisible(foreground.target),
            targetUnoccluded,
            modalChildPresent,
        });
        let reason = null;
        if (!targetSafe && currentTarget !== foreground.target)
            reason = 'stale_target';
        else if (!targetSafe && global.display.focus_window !== foreground.target)
            reason = 'focus_changed';
        else if (!targetSafe && !this._isTargetVisible(foreground.target))
            reason = 'target_not_visible';
        else if (!targetSafe && modalChildPresent)
            reason = 'child_modal_present';
        else if (!targetSafe && !targetUnoccluded)
            reason = 'target_occluded';
        if (reason)
            return JSON.stringify({valid: false, terminal: false, state: 'reconciling', reason});
        return JSON.stringify({
            valid: true,
            target: foreground.targetId,
            transaction,
        });
    }

    EndForegroundAsync([transaction], invocation) {
        this._finishForegroundAsync(transaction, 'complete', result => {
            invocation.return_value(new GLib.Variant('(s)', [JSON.stringify(result)]));
        });
    }

    AbortForegroundAsync([transaction], invocation) {
        this._finishForegroundAsync(transaction, 'aborted', result => {
            invocation.return_value(new GLib.Variant('(s)', [JSON.stringify(result)]));
        });
    }

    CommitForeground(transaction) {
        const foreground = this._foreground;
        if (!foreground || foreground.transaction !== transaction)
            return JSON.stringify({committed: false, reason: 'stale_transaction'});
        this._foreground = null;
        if (foreground.timeoutId) {
            try { GLib.source_remove(foreground.timeoutId); } catch (_error) {}
        }
        return JSON.stringify({committed: true});
    }

    _finishForegroundAsync(transaction, reason, callback) {
        const foreground = this._foreground;
        if (!foreground || foreground.transaction !== transaction)
            return callback?.({terminal: true, state: 'terminal', restored: false, reason: 'stale_transaction'});
        if (foreground.state === 'restoring')
            return callback?.({terminal: false, state: 'restoring', restored: false, reason: 'restoration_in_progress'});
        foreground.state = 'restoring';
        if (foreground.timeoutId) {
            try { GLib.source_remove(foreground.timeoutId); } catch (_error) {}
            foreground.timeoutId = 0;
        }

        const complete = result => {
            if (this._foreground?.transaction === transaction)
                this._foreground = null;
            callback?.({
                terminal: true,
                state: 'terminal',
                activation_required: foreground.activationRequired,
                target_activation_verified: foreground.activated,
                ...result,
            });
        };

        const currentFocus = global.display.focus_window;
        if (currentFocus && currentFocus !== foreground.target) {
            complete({
                restored: false,
                restoration_attempted: false,
                restoration_succeeded: false,
                outcome: 'preserved_user_context',
                reason: 'user_focus_changed',
            });
            return;
        }

        const windows = global.get_window_actors().map(actor => actor.meta_window);
        if (foreground.priorWindow && foreground.priorWindow !== foreground.target &&
            windows.includes(foreground.priorWindow)) {
            foreground.priorWindow.activate(global.get_current_time());
            GLib.timeout_add(GLib.PRIORITY_DEFAULT, 100, () => {
                const succeeded = global.display.focus_window === foreground.priorWindow;
                complete({
                    restored: succeeded,
                    restoration_attempted: true,
                    restoration_succeeded: succeeded,
                    outcome: succeeded ? 'restored_prior_context' : 'restoration_unresolved',
                    reason: succeeded ? reason : 'prior_window_not_confirmed',
                    restored_to: 'window',
                });
                return GLib.SOURCE_REMOVE;
            });
            return;
        }
        if (foreground.priorWindow === foreground.target) {
            complete({
                restored: false,
                restoration_attempted: false,
                restoration_succeeded: true,
                outcome: 'no_activation_required',
                reason: 'target_was_already_focused',
            });
            return;
        }
        try {
            if (!foreground.priorWorkspace)
                throw new Error('missing prior workspace');
            foreground.priorWorkspace.activate(global.get_current_time());
            GLib.timeout_add(GLib.PRIORITY_DEFAULT, 100, () => {
                const succeeded = global.workspace_manager.get_active_workspace() === foreground.priorWorkspace;
                complete({
                    restored: succeeded,
                    restoration_attempted: true,
                    restoration_succeeded: succeeded,
                    outcome: succeeded ? 'restored_prior_context' : 'restoration_unresolved',
                    reason: succeeded ? reason : 'prior_workspace_not_confirmed',
                    restored_to: 'workspace',
                });
                return GLib.SOURCE_REMOVE;
            });
        } catch (_error) {
            complete({
                restored: false,
                restoration_attempted: true,
                restoration_succeeded: false,
                outcome: 'restoration_unresolved',
                reason: 'prior_context_unavailable',
            });
        }
    }

    _cursorOwner(owner, invocation) {
        const connectionOwner = invocation.get_sender();
        if (
            typeof connectionOwner !== 'string'
            || !connectionOwner.startsWith(':')
            || typeof owner !== 'string'
            || owner.length < 1
            || owner.length > 256
        )
            throw new Error('cursor_owner_invalid: a live D-Bus connection and bounded label are required');
        return {
            connectionOwner,
            key: JSON.stringify([connectionOwner, owner]),
        };
    }

    _cursorFor(owner) {
        let record = this._cursors.get(owner.key);
        if (!record) {
            record = this._createCursorRecord(owner);
            this._cursors.set(owner.key, record);
        }
        record.lastUsedAt = GLib.get_monotonic_time();
        return record;
    }

    _bindCursorTarget(record, target) {
        if (record.targetWindow === target)
            return;
        for (const [object, id] of record.targetSignals) {
            try { object.disconnect(id); } catch (_error) {}
        }
        record.targetSignals = [];
        record.targetWindow = target;
        if (!target)
            return;
        const update = () => this._scheduleCursorVisibilityUpdate();
        for (const signal of ['workspace-changed', 'notify::minimized', 'unmanaged']) {
            try { record.targetSignals.push([target, target.connect(signal, update)]); } catch (_error) {}
        }
        const actor = this._actorFor(target);
        try { record.targetSignals.push([actor, actor.connect('notify::visible', update)]); } catch (_error) {}
    }

    _syncCursorVisibility(record) {
        const target = record.targetId ? this._resolveTarget(record.targetId) : null;
        this._bindCursorTarget(record, target);
        const visible = record.requestedVisible && target && this._isTargetVisible(target);
        if (visible) {
            record.actor.show();
            this._updateCursorBadge(record);
        } else {
            record.actor.hide();
            record.badge.hide();
        }
    }

    _scheduleCursorVisibilityUpdate() {
        if (this._cursorUpdatePending)
            return;
        this._cursorUpdatePending = true;
        GLib.idle_add(GLib.PRIORITY_DEFAULT_IDLE, () => {
            this._cursorUpdatePending = false;
            for (const record of this._cursors.values())
                this._syncCursorVisibility(record);
            return GLib.SOURCE_REMOVE;
        });
    }

    MoveCursorForAsync([owner, targetId, x, y], invocation) {
        try {
            const record = this._cursorFor(this._cursorOwner(owner, invocation));
            record.targetId = targetId;
            record.requestedVisible = true;
            record.cursorX = x; record.cursorY = y;
            this._syncCursorVisibility(record);
            record.actor.ease({
                x: x - ACTOR_CENTER,
                y: y - ACTOR_CENTER,
                duration: 480,
                mode: Clutter.AnimationMode.EASE_OUT_CUBIC,
            });
            this._positionBadge(record, x, y, 480);
            invocation.return_value(null);
        } catch (error) {
            invocation.return_dbus_error('org.cua.WinRects.CursorRejected', String(error));
        }
    }

    ClickPulseForAsync([owner, targetId, x, y], invocation) {
        try {
            const record = this._cursorFor(this._cursorOwner(owner, invocation));
            record.targetId = targetId;
            record.requestedVisible = true;
            record.actor.set_position(x - ACTOR_CENTER, y - ACTOR_CENTER);
            record.cursorX = x; record.cursorY = y;
            this._positionBadge(record, x, y, 0);
            if (['idle', 'navigate', 'click'].includes(record.action))
                this._setRecordCursorState(record, 'click', record.delivery, record.target);
            this._syncCursorVisibility(record);
            record.actor.ease({
                scale_x: 1.5,
                scale_y: 1.5,
                duration: 130,
                mode: Clutter.AnimationMode.EASE_OUT_QUAD,
                onComplete: () => {
                    if (record.actor)
                        record.actor.ease({scale_x: 1, scale_y: 1, duration: 130});
                },
            });
            invocation.return_value(null);
        } catch (error) {
            invocation.return_dbus_error('org.cua.WinRects.CursorRejected', String(error));
        }
    }

    _semanticCursorFor(owner, invocation) {
        return this._cursorFor(this._cursorOwner(owner, invocation));
    }

    _setCursorColor(owner, fillColor, invocation) {
        const record = this._semanticCursorFor(owner, invocation);
        const match = /^#([0-9a-fA-F]{6})$/.exec(String(fillColor));
        if (!match) throw new Error('cursor_color_invalid: expected #RRGGBB');
        const rgb = Number.parseInt(match[1], 16);
        record.fillColor = [(rgb >> 16) & 0xff, (rgb >> 8) & 0xff, rgb & 0xff];
        record.fillColorCss = `#${match[1].toLowerCase()}`;
        record.badge.set_style(badgeStyle(record.fillColor));
        if (record.glowSurface) record.glowSurface.finish();
        record.glowSurface = createGlowSurface(record.fillColor);
        record.deliveryChip.queue_repaint(); record.targetChip.queue_repaint(); record.actor.queue_repaint();
    }

    _setCursorState(owner, action, delivery, target, active, invocation) {
        const record = this._semanticCursorFor(owner, invocation);
        if (!ACTIONS.has(action)) throw new Error('cursor_action_invalid');
        if (active) this._setRecordCursorState(record, action, delivery, target);
        else if (record.action === action && !ONE_SHOT_ACTIONS.has(action))
            record.endingAt = nowSeconds() + 0.4;
    }

    _setSessionLabel(owner, label, invocation) {
        const record = this._semanticCursorFor(owner, invocation);
        record.sessionLabel = String(label).slice(0, 24);
        record.badgeLabel.set_text(record.sessionLabel);
        if (!record.sessionLabel || !record.actor.visible) {
            record.badgeLabel.hide(); record.badgeRevealedAt = null;
            return;
        }
        record.badgeLabel.show();
        record.badgeRevealedAt = nowSeconds();
        if (record.cursorX !== null && record.cursorY !== null)
            this._positionBadge(record, record.cursorX, record.cursorY, 0);
    }

    SetCursorColorAsync([fillColor], invocation) {
        try { this._setCursorColor('default', fillColor, invocation); invocation.return_value(null); }
        catch (error) { invocation.return_dbus_error('org.cua.WinRects.CursorRejected', String(error)); }
    }
    SetCursorStateAsync([action, delivery, target, active], invocation) {
        try { this._setCursorState('default', action, delivery, target, active, invocation); invocation.return_value(null); }
        catch (error) { invocation.return_dbus_error('org.cua.WinRects.CursorRejected', String(error)); }
    }
    SetSessionLabelAsync([label], invocation) {
        try { this._setSessionLabel('default', label, invocation); invocation.return_value(null); }
        catch (error) { invocation.return_dbus_error('org.cua.WinRects.CursorRejected', String(error)); }
    }
    SetCursorColorForAsync([owner, fillColor], invocation) {
        try { this._setCursorColor(owner, fillColor, invocation); invocation.return_value(null); }
        catch (error) { invocation.return_dbus_error('org.cua.WinRects.CursorRejected', String(error)); }
    }
    SetCursorStateForAsync([owner, action, delivery, target, active], invocation) {
        try { this._setCursorState(owner, action, delivery, target, active, invocation); invocation.return_value(null); }
        catch (error) { invocation.return_dbus_error('org.cua.WinRects.CursorRejected', String(error)); }
    }
    SetSessionLabelForAsync([owner, label], invocation) {
        try { this._setSessionLabel(owner, label, invocation); invocation.return_value(null); }
        catch (error) { invocation.return_dbus_error('org.cua.WinRects.CursorRejected', String(error)); }
    }

    HideCursorForAsync([owner], invocation) {
        try {
            const cursorOwner = this._cursorOwner(owner, invocation);
            const record = this._cursors.get(cursorOwner.key);
            if (record) {
                record.lastUsedAt = GLib.get_monotonic_time();
                record.requestedVisible = false;
                record.actor.hide();
                record.badge.hide();
                record.badgeRevealedAt = null;
                record.modifierFadeAt = null;
            }
            invocation.return_value(null);
        } catch (error) {
            invocation.return_dbus_error('org.cua.WinRects.CursorRejected', String(error));
        }
    }

    RemoveCursorAsync([owner], invocation) {
        try {
            const cursorOwner = this._cursorOwner(owner, invocation);
            this._removeCursor(cursorOwner.key);
            invocation.return_value(null);
        } catch (error) {
            invocation.return_dbus_error('org.cua.WinRects.CursorRejected', String(error));
        }
    }

    _removeCursorsForConnection(connectionOwner) {
        for (const [owner, record] of [...this._cursors]) {
            if (record.connectionOwner === connectionOwner)
                this._removeCursor(owner);
        }
    }

    _removeCursor(owner) {
        const record = this._cursors.get(owner);
        if (!record)
            return;
        for (const [object, id] of record.targetSignals) {
            try { object.disconnect(id); } catch (_error) {}
        }
        if (record.frameId) GLib.source_remove(record.frameId);
        if (record.glowSurface) record.glowSurface.finish();
        record.actor.destroy();
        record.badge.destroy();
        this._cursors.delete(owner);
    }

    // WinRects v1 compatibility methods fail closed. Keeping the D-Bus
    // signatures makes version skew deterministic rather than accidentally
    // falling through to global Shell chrome or unqualified activation.
    CaptureAsync(_params, invocation) {
        invocation.return_dbus_error(
            'org.cua.WinRects.UpgradeRequired',
            'background_unavailable: WinRects protocol v2 requires exact target capture'
        );
    }
    ActivateAsync(_params, invocation) {
        invocation.return_value(new GLib.Variant('(b)', [false]));
    }
    MoveCursor(_x, _y) {}
    ClickPulse(_x, _y) {}
    HideCursor() {}
}
