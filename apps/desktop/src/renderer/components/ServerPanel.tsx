import { type FormEvent, useEffect, useState } from 'react';

import type { SignalingSettings } from '../../../shared/protocol';
import { bridge } from '../lib/bridge';

export function ServerPanel({ onClose }: { onClose: () => void }) {
  const [current, setCurrent] = useState<SignalingSettings | null>(null);
  const [draft, setDraft] = useState('');
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<{ error: boolean; text: string } | null>(null);

  useEffect(() => {
    let alive = true;
    void bridge.getSignalingSettings().then((s) => {
      if (!alive || !s) return;
      setCurrent(s);
      setDraft(s.custom ? s.url : '');
    });
    return () => {
      alive = false;
    };
  }, []);

  const apply = async (url: string | null) => {
    setBusy(true);
    setMessage(null);
    try {
      const res = await bridge.setSignalingUrl(url);
      if (!res.ok) {
        setMessage({ error: true, text: res.error });
        return;
      }
      const next = await bridge.getSignalingSettings();
      if (next) {
        setCurrent(next);
        setDraft(next.custom ? next.url : '');
      }
      setMessage({ error: false, text: `Kaydedildi. Sunucu: ${res.url}` });
    } finally {
      setBusy(false);
    }
  };

  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (draft.trim()) void apply(draft);
  };

  return (
    <section className="card server-panel">
      <header className="card-header">
        <h2>Sunucu adresi</h2>
        <p className="muted">
          Bağlanacak iki cihaz da aynı sunucuyu kullanmalı. Değiştirmek açık oturumları kapatır.
        </p>
      </header>
      <form className="connect-form" onSubmit={submit}>
        <label>
          <span className="label">Signaling sunucusu</span>
          <input
            className="mono"
            autoComplete="off"
            spellCheck={false}
            placeholder={current?.defaultUrl ?? 'wss://…/ws'}
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
          />
        </label>
        <div className="row">
          <button className="primary" type="submit" disabled={busy || !draft.trim()}>
            {busy ? 'Uygulanıyor…' : 'Kaydet'}
          </button>
          <button className="ghost" type="button" disabled={busy || !current?.custom} onClick={() => void apply(null)}>
            Varsayılana dön
          </button>
          <button className="ghost" type="button" onClick={onClose}>
            Kapat
          </button>
        </div>
      </form>
      {current && <p className="muted">Şu an kullanılan: <span className="mono">{current.url}</span></p>}
      {message && <p className={message.error ? 'error-text' : 'muted'}>{message.text}</p>}
    </section>
  );
}
