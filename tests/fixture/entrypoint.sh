#!/bin/sh
# Runs inside the fixture container (fixture.sh starts it): makes the TLS files and the mailbox on first start,
# then runs Dovecot in the foreground. /state survives restarts; /fixture is this directory, read-only.
set -eu

if [ ! -f /state/tls/server.pem ]; then
  mkdir -p /state/tls
  cd /state/tls
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 3650 -subj "/CN=kubicle fixture CA" \
    -addext basicConstraints=critical,CA:TRUE -addext keyUsage=critical,keyCertSign,cRLSign \
    -keyout ca.key -out ca.pem 2>/dev/null
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj "/CN=imap.example.test" \
    -keyout server.key -out server.csr 2>/dev/null
  printf 'subjectAltName=DNS:imap.example.test\nbasicConstraints=CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n' >ext.cnf
  openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 3650 -extfile ext.cnf \
    -out server.pem 2>/dev/null
  chmod 0644 ca.pem server.pem
fi

if [ ! -d /state/mail ]; then
  python3 -I /fixture/seed.py /state/mail/andrew@example.test
  chown -R mail:mail /state/mail
fi

exec dovecot -F -c /fixture/dovecot.conf
