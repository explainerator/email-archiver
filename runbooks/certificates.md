# Certificate renewal

Automatic — certbot's systemd timer, with a renewal hook that keeps the certificate
readable by the service. The archiver re-reads it when the file changes, so renewal needs
no restart.

**Port 80 must stay free.** `certbot --standalone` binds it at *every* renewal, not only at
issuance. Nothing listens there by design; putting an HTTP redirect on it would work for
about ninety days and then fail silently.

`status` shows days remaining. If it stops falling, renewal has stopped working:

```
ssh ubuntu@<instance> 'sudo certbot renew --dry-run'
```
