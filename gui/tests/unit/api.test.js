import { describe, it, expect, vi, beforeEach } from 'vitest';
import { apiFetch, logIn, storedToken, LOGGED_OUT_EVENT } from '../../src/api.js';

const TOKEN = 'header.payload.signature';

function jsonResponse(status, body) {
  return new Response(JSON.stringify(body), { status });
}

describe('apiFetch', () => {
  beforeEach(() => {
    localStorage.clear();
    vi.unstubAllGlobals();
  });

  it('sends no Authorization header before login', async () => {
    const fetchMock = vi.fn(async () => jsonResponse(200, []));
    vi.stubGlobal('fetch', fetchMock);
    await apiFetch('/api/v1/assets');
    expect(fetchMock.mock.calls[0][1].headers.has('Authorization')).toBe(false);
  });

  it('sends the login token as a bearer on every call', async () => {
    const fetchMock = vi.fn(async () => jsonResponse(200, { token: TOKEN }));
    vi.stubGlobal('fetch', fetchMock);
    await logIn('a@example.com', 'secret');
    await apiFetch('/api/v1/assets', { method: 'POST', headers: { 'X-Other': '1' } });

    const [, options] = fetchMock.mock.calls[1];
    expect(options.method).toBe('POST');
    expect(options.headers.get('Authorization')).toBe(`Bearer ${TOKEN}`);
    expect(options.headers.get('X-Other')).toBe('1');
  });

  it('drops a token the server refuses and announces the logout', async () => {
    localStorage.setItem('tiletopia-token', TOKEN);
    vi.stubGlobal('fetch', vi.fn(async () => jsonResponse(401, {})));
    const loggedOut = vi.fn();
    window.addEventListener(LOGGED_OUT_EVENT, loggedOut);

    await apiFetch('/api/v1/assets');

    expect(storedToken()).toBeNull();
    expect(loggedOut).toHaveBeenCalledOnce();
    window.removeEventListener(LOGGED_OUT_EVENT, loggedOut);
  });

  it('keeps no token when login fails', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => jsonResponse(401, {})));
    await expect(logIn('a@example.com', 'wrong')).rejects.toThrow('401');
    expect(storedToken()).toBeNull();
  });
});
