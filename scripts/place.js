#!/usr/bin/env node
/* SPDX-License-Identifier: MIT
 *
 * A stand-in shell, for driving the compositor before there is a web engine.
 *
 * Speaks the same JSON protocol data/shell/*.js speaks, over the same control
 * socket, and does the one thing the real shell does that the compositor
 * cannot do for itself: decide where windows go. Windows are tiled left to
 * right in the order they appear.
 *
 *   node scripts/place.js [socket-path]
 *
 * Without an argument it uses $VIEWPORT_SOCKET, then
 * $XDG_RUNTIME_DIR/viewport-$WAYLAND_DISPLAY.sock.
 */

const net = require('node:net');

const path =
  process.argv[2] ||
  process.env.VIEWPORT_SOCKET ||
  `${process.env.XDG_RUNTIME_DIR}/viewport-${process.env.WAYLAND_DISPLAY}.sock`;

const GAP = 12;

let layout = { x: 0, y: 0, width: 1280, height: 720 };
const order = [];

const socket = net.createConnection(path);
let buffer = '';

socket.on('connect', () => {
  console.log(`connected to ${path}`);
  send({ type: 'output.query' });
  send({ type: 'view.query' });
});

socket.on('data', (chunk) => {
  buffer += chunk;
  let index;
  while ((index = buffer.indexOf('\n')) >= 0) {
    const line = buffer.slice(0, index);
    buffer = buffer.slice(index + 1);
    if (line) handle(JSON.parse(line));
  }
});

socket.on('error', (e) => {
  console.error(`${path}: ${e.message}`);
  process.exit(1);
});

function send(message) {
  socket.write(`${JSON.stringify(message)}\n`);
}

function handle(message) {
  switch (message.type) {
    case 'output.layout': {
      const output = message.outputs[0];
      if (output) {
        layout = {
          x: output.usable_x,
          y: output.usable_y,
          width: output.usable_width,
          height: output.usable_height,
        };
        console.log(
          `output ${output.name} ${layout.width}x${layout.height}+${layout.x}+${layout.y}`,
        );
        retile();
      }
      break;
    }

    case 'view.added':
      console.log(
        `+ view ${message.id} "${message.title}" (${message.app_id})` +
          `${message.replay ? ' [replay]' : ''}` +
          `${message.floating ? ' [floating]' : ''}`,
      );
      if (!order.includes(message.id)) order.push(message.id);
      retile();
      break;

    case 'view.removed': {
      console.log(`- view ${message.id}`);
      const at = order.indexOf(message.id);
      if (at >= 0) order.splice(at, 1);
      retile();
      break;
    }

    case 'view.props':
      console.log(`~ view ${message.id} "${message.title}" (${message.app_id})`);
      break;

    case 'view.focused':
      console.log(`focus -> ${message.id === 0 ? 'shell' : `view ${message.id}`}`);
      break;

    case 'error':
      console.error(`error [${message.context}] ${message.message}`);
      break;

    default:
      console.log(`. ${message.type}`);
  }
}

/* Columns, left to right. The compositor has no opinion about any of this —
 * that is the point. */
function retile() {
  if (order.length === 0) return;
  const width = Math.floor(
    (layout.width - GAP * (order.length + 1)) / order.length,
  );
  order.forEach((id, i) => {
    send({
      type: 'view.layout',
      id,
      x: layout.x + GAP + i * (width + GAP),
      y: layout.y + GAP,
      width,
      height: layout.height - GAP * 2,
    });
  });
  console.log(`laid out ${order.length} view(s) at ${width}px wide`);
}
