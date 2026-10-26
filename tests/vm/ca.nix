# A test CA and one certificate for the mail host under every name the tests use: imap.example.test as in the
# Dovecot fixture, mail.example.test as Mailu's HOSTNAMES. Made at build time; the key is a test key.
{
  openssl,
  runCommand,
}:
runCommand "kubicle-test-ca" {nativeBuildInputs = [openssl];} ''
  mkdir $out && cd $out
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 3650 -subj "/CN=kubicle test CA" \
    -addext basicConstraints=critical,CA:TRUE -addext keyUsage=critical,keyCertSign,cRLSign \
    -keyout ca.key -out ca.pem
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj "/CN=mail.example.test" \
    -keyout server.key -out server.csr
  printf '%s\n' subjectAltName=DNS:mail.example.test,DNS:imap.example.test basicConstraints=CA:FALSE \
    keyUsage=critical,digitalSignature extendedKeyUsage=serverAuth >ext.cnf
  openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 3650 -extfile ext.cnf \
    -out server.pem
  rm ca.key server.csr ext.cnf ca.srl
''
