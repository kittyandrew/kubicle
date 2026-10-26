# The live scenarios (liveness.py) as checks that run side by side, each VM signed in on a small mailbox. Three
# VMs fit the RAM a test run may take (each holds about 1.6 GB on the host, mostly the loaded images), so scenarios
# that cannot disturb each other share one, and the IMAP cut, which can stall everything after it, runs alone.
{
  pkgs,
  kubicle,
}: let
  judge = import ../judge.nix {inherit pkgs;};
  mailbox = pkgs.callPackage ./mailbox.nix {};
  mailu = import ./mailu.nix {inherit pkgs kubicle;} {
    mail = "${mailbox}/small.tar.zst";
    memorySize = 1536; # measured peak in use (MemTotal - MemAvailable): 1098 MB
  };
  check = name: scenarios:
    judge (pkgs.testers.runNixOSTest {
      name = "kubicle-${name}";
      nodes.machine = mailu.node;
      testScript =
        builtins.readFile ../record.py
        + builtins.readFile ./scenarios.py
        + builtins.readFile ./liveness.py
        + mailu.setup
        + ''
          run_tier(("sign-in through sso.php", sign_in), *[LIVE[name] for name in ${builtins.toJSON scenarios}], shared=False)
        '';
    });
in
  builtins.mapAttrs (name: check name) {
    # Keys of LIVE in liveness.py.
    vm-live-mail = ["delivery" "second-client"];
    vm-live-offline = ["offline" "dovecot-restart"];
    vm-live-imap-cut = ["imap-cut"];
  }
