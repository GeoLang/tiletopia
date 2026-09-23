const TOKEN_STORAGE_KEY = 'tiletopia-token';
const UNAUTHORIZED_STATUS = 401;
export const LOGGED_OUT_EVENT = 'tiletopia-logged-out';

export function storedToken() {
  return localStorage.getItem(TOKEN_STORAGE_KEY);
}

export function forgetToken() {
  localStorage.removeItem(TOKEN_STORAGE_KEY);
}

export async function logIn(email, password) {
  const res = await fetch('/api/v1/auth/login', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ email, password }),
  });
  if (!res.ok) throw new Error(`login answered ${res.status}`);
  const { token } = await res.json();
  localStorage.setItem(TOKEN_STORAGE_KEY, token);
}

export async function apiFetch(url, options = {}) {
  const headers = new Headers(options.headers);
  const token = storedToken();
  if (token) headers.set('Authorization', `Bearer ${token}`);
  const res = await fetch(url, { ...options, headers });
  // an expired token is dropped so the login form comes back
  if (res.status === UNAUTHORIZED_STATUS && token) {
    forgetToken();
    window.dispatchEvent(new Event(LOGGED_OUT_EVENT));
  }
  return res;
}
