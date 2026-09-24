import { describe, it, expect, vi, beforeEach } from 'vitest';
import { CollaborationPanel } from '../../src/collaboration.js';

vi.mock('cesium', () => ({
  default: {},
  ScreenSpaceEventHandler: class {
    setInputAction() {}
  },
  ScreenSpaceEventType: { MOUSE_MOVE: 'MOUSE_MOVE' },
}));

const MARKUP_NAME = '<img src=x onerror="window.injected=true">';

class FakeWebSocket {
  static OPEN = 1;
  static last = null;

  constructor() {
    this.readyState = FakeWebSocket.OPEN;
    this.listeners = {};
    FakeWebSocket.last = this;
  }

  addEventListener(type, listener) {
    this.listeners[type] = listener;
  }

  send() {}

  close() {}

  receive(message) {
    this.listeners.message({ data: JSON.stringify(message) });
  }
}

function connectedPanel() {
  const viewer = { scene: { canvas: {} }, entities: { add() {}, remove() {} } };
  const panel = new CollaborationPanel(viewer);
  panel.connect('room-1');
  return FakeWebSocket.last;
}

describe('CollaborationPanel', () => {
  beforeEach(() => {
    document.body.innerHTML = '';
    vi.stubGlobal('WebSocket', FakeWebSocket);
  });

  it('shows a member name from presence as text, never as markup', () => {
    connectedPanel().receive({
      type: 'Presence',
      users: [{ user_id: 'other', user_name: MARKUP_NAME, color: '#e06c75' }],
    });

    const list = document.getElementById('collab-users');
    expect(list.querySelector('img')).toBeNull();
    expect(list.textContent).toContain(MARKUP_NAME);
  });

  it('shows a chat sender name and message as text, never as markup', () => {
    connectedPanel().receive({
      type: 'Chat',
      user_id: 'other',
      user_name: MARKUP_NAME,
      message: MARKUP_NAME,
      timestamp: '2026-09-24T00:00:00Z',
    });

    const log = document.getElementById('collab-chat-log');
    expect(log.querySelector('img')).toBeNull();
    expect(log.textContent).toBe(`${MARKUP_NAME}: ${MARKUP_NAME}`);
  });
});
