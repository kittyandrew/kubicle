# Imported by every VM the tests boot. Used memory, kubicle's share of it and load in the journal every 2 s, so a
# run short of memory or CPU shows when and how far, and the console carries them too for a VM that crashed
# (tests/record.py collects the journal).
{
  lib,
  pkgs,
  ...
}: {
  # The test framework copies every journal line to the serial console, which ran the hostile suite at half speed
  # once kubicle logged each request: a unit that sets `SyslogLevel = "debug"` stays in the journal alone.
  services.journald.settings.Journal.MaxLevelConsole = lib.mkForce "info";
  systemd.services.capture-load = {
    wantedBy = ["multi-user.target"];
    path = [pkgs.procps pkgs.gawk pkgs.uutils-coreutils-noprefix];
    script = ''
      while true; do
        kubicle=$(ps -C kubicle-server -o rss= | awk '{kb += $1} END {print int(kb / 1024)}')
        used=$(free -m | awk '/Mem:/ {print $3}')
        echo "load $(cut -d' ' -f1-3 /proc/loadavg), used $used MB, kubicle $kubicle MB"
        sleep 2
      done
    '';
  };
}
