import { bridge } from '../lib/bridge';
import { type AppState, formatBitrate, formatDeviceId } from '../state';

interface Props {
  state: AppState;
  onAllowControl(allowed: boolean): void;
}

export function HostPanel({ state, onAllowControl }: Props) {
  const session = state.hostSession;
  const active = session && session.state !== 'ended';

  return (
    <section className="card">
      <header className="card-header">
        <h2>Bu cihaza erişim</h2>
        <p className="muted">Kimliği ve tek kullanımlık şifreyi paylaştığınız kişi bu ekranı görebilir.</p>
      </header>

      {state.hosting ? (
        <div className="credentials">
          <div>
            <span className="label">Kimlik</span>
            <span className="mono big">{formatDeviceId(state.deviceId)}</span>
          </div>
          <div>
            <span className="label">Şifre</span>
            <span className="mono big">{state.password}</span>
          </div>
          <button className="ghost" onClick={() => bridge.send({ type: 'host.rotatePassword' })}>
            Yeni şifre
          </button>
        </div>
      ) : (
        <button
          className="primary"
          disabled={!state.agentReady}
          onClick={() => bridge.send({ type: 'host.start', allowControl: state.allowControl })}
        >
          Erişime aç
        </button>
      )}

      <label className="toggle">
        <input
          type="checkbox"
          checked={state.allowControl}
          onChange={(e) => {
            onAllowControl(e.target.checked);
            bridge.send({ type: 'host.setControl', allowed: e.target.checked });
          }}
        />
        <span>Uzaktan fare ve klavye kontrolüne izin ver</span>
      </label>

      {active && (
        <div className="session-banner">
          <span className={`dot ${session.state}`} />
          <span>
            {session.state === 'connected' ? 'Bağlı' : 'Bağlanıyor'}: <b className="mono">{formatDeviceId(session.peer)}</b>
            {session.transport && <span className="pill">{session.transport === 'quic' ? 'P2P QUIC' : 'Relay (TCP)'}</span>}
          </span>
          {state.hostStats && (
            <span className="muted">
              {formatBitrate(state.hostStats.bitrateBps)} · {state.hostStats.fps} fps · {state.hostStats.rttMs} ms
            </span>
          )}
        </div>
      )}

      {state.hosting && (
        <button className="danger ghost" onClick={() => bridge.send({ type: 'host.stop' })}>
          Erişimi kapat
        </button>
      )}
    </section>
  );
}
