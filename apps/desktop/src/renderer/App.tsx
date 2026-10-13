import { useEffect, useReducer } from 'react';

import { ConnectPanel } from './components/ConnectPanel';
import { HostPanel } from './components/HostPanel';
import { RemoteView } from './components/RemoteView';
import { bridge } from './lib/bridge';
import { initialState, reducer } from './state';

export function App() {
  const [state, dispatch] = useReducer(reducer, initialState);

  useEffect(() => bridge.onEvent(dispatch), []);

  const viewing = state.viewerSession && state.viewerSession.state !== 'ended';
  const needsPermissions = bridge.platform === 'darwin' && state.permissions && (!state.permissions.screen || !state.permissions.input);

  return (
    <div className="app">
      <header className="topbar">
        <span className="brand">Remoti</span>
        <span className={`status ${state.online ? 'online' : 'offline'}`}>
          <span className="dot" />
          {state.online ? 'Çevrimiçi' : state.agentReady ? 'Sunucuya bağlanıyor…' : 'Başlatılıyor…'}
        </span>
      </header>

      {state.error && (
        <div className="toast" role="alert">
          <span>{state.error.message}</span>
          <button className="ghost" onClick={() => dispatch({ type: 'local.dismissError' })}>
            Kapat
          </button>
        </div>
      )}

      {needsPermissions && (
        <div className="notice">
          <span>Ekranınızı paylaşmak için macOS izinleri gerekli.</span>
          {!state.permissions?.screen && (
            <button className="ghost" onClick={() => bridge.openPrivacySettings('screen')}>
              Ekran Kaydı
            </button>
          )}
          {!state.permissions?.input && (
            <button className="ghost" onClick={() => bridge.openPrivacySettings('accessibility')}>
              Erişilebilirlik
            </button>
          )}
          <button className="ghost" onClick={() => bridge.send({ type: 'permissions.request' })}>
            İzin iste
          </button>
        </div>
      )}

      {viewing ? (
        <RemoteView state={state} />
      ) : (
        <main className="panels">
          <HostPanel state={state} onAllowControl={(allowed) => dispatch({ type: 'local.allowControl', allowed })} />
          <ConnectPanel state={state} />
        </main>
      )}
    </div>
  );
}
