Test-only TLS material for the RetroAchievements fake server (0.6.0 Phase 3).

- `ca.pem`: a throwaway certificate authority (its key was deleted after signing).
- `server.pem` / `server.key`: a certificate for the made-up host `ra.test`, signed by it.

Never trusted outside tests: production uses Mozilla's roots (`webpki-roots`).
