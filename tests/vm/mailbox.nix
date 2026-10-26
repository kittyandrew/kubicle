# The fixture mailbox, seeded once at build time, as two tarballs: full.tar.zst is all of seed.py's mail, and
# small.tar.zst keeps each folder's newest 200 messages, for tests that need live mail rather than volume.
# Tarballs because the store resets file times, and a Maildir file's time is its message's arrival date.
{
  python3,
  runCommand,
  zstd,
}:
runCommand "kubicle-fixture-mailbox" {nativeBuildInputs = [python3 zstd];} ''
  python3 -I ${../fixture/seed.py} mail
  mkdir $out
  tar -C mail --zstd -cf $out/full.tar.zst .
  for cur in mail/cur mail/.[!.]*/cur; do
    ls "$cur" | sort | head -n -200 | sed "s|^|$cur/|" | xargs -r rm
  done
  tar -C mail --zstd -cf $out/small.tar.zst .
''
