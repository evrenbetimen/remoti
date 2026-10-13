import { type FormEvent, useState } from 'react';

import { bridge } from '../lib/bridge';
import { decodableCodecs } from '../lib/codecSupport';
import type { AppState } from '../state';

export function ConnectPanel({ state }: { state: AppState }) {
  const [deviceId, setDeviceId] = useState('');
  const [password, setPassword] = useState('');
  const session = state.viewerSession;
  const busy = session?.state === 'connecting' || session?.state === 'reconnecting';
  const digits = deviceId.replace(/\D/g, '');

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    const secret = password.trim();
    if (digits.length !== 9 || !secret) return;
    // Never keep the one-time password around longer than needed.
    setPassword('');
    const codecs = await decodableCodecs();
    bridge.send({ type: 'viewer.connect', deviceId: digits, password: secret, codecs });
  };

  return (
    <section className="card">
      <header className="card-header">
        <h2>Uzak cihaza bağlan</h2>
        <p className="muted">Bağlantı uçtan uca şifrelenir; sunucu görüntüyü veya girdiyi göremez.</p>
      </header>
      <form className="connect-form" onSubmit={(e) => void submit(e)}>
        <label>
          <span className="label">Ortak kimlik</span>
          <input
            className="mono"
            inputMode="numeric"
            autoComplete="off"
            placeholder="123 456 789"
            value={deviceId}
            maxLength={11}
            onChange={(e) => setDeviceId(e.target.value)}
          />
        </label>
        <label>
          <span className="label">Şifre</span>
          <input
            className="mono"
            type="password"
            autoComplete="off"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
          />
        </label>
        <button className="primary" type="submit" disabled={!state.online || busy || digits.length !== 9 || !password.trim()}>
          {busy ? (session?.state === 'reconnecting' ? 'Yeniden bağlanıyor…' : 'Bağlanıyor…') : 'Bağlan'}
        </button>
      </form>
      {session?.state === 'ended' && session.reason && <p className="error-text">{session.reason}</p>}
    </section>
  );
}
