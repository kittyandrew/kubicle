# vm-settings: kubicle against Roundcube, the webmail it copies, on combinations of the settings both have
# (differential.py). Mailu runs both webmails, each with its own account on the same small mailbox.
{
  pkgs,
  kubicle,
}: let
  judge = import ../judge.nix {inherit pkgs;};
  mailbox = pkgs.callPackage ./mailbox.nix {};
  ca = pkgs.callPackage ./ca.nix {};
  mailu = import ./mailu.nix {inherit pkgs kubicle;} {
    mail = "${mailbox}/small.tar.zst";
    memorySize = 3072;
    roundcube = true;
  };
  mailState = pkgs.writers.writePython3Bin "mail-state" {flakeIgnore = ["E501"];} (builtins.readFile ./mailstate.py);
in
  judge (pkgs.testers.runNixOSTest {
    name = "kubicle-vm-settings";
    nodes.machine = {
      imports = [mailu.node];
      # kubicle's "Auto" zone is the browser's, Roundcube's the server's (docs/design.md, Time zone): the same
      # zone for both, as Roundcube's PHP and kubicle's server run in Europe/Kyiv (mailu.nix).
      systemd.services.chromedriver.environment.TZ = "Europe/Kyiv";
      # The test drives both browsers from the host through a forwarded port; chromedriver listens on loopback only.
      systemd.services.chromedriver-relay = {
        wantedBy = ["multi-user.target"];
        serviceConfig.ExecStart = "${pkgs.lib.getExe pkgs.socat} TCP-LISTEN:9516,fork,reuseaddr TCP:127.0.0.1:9515";
      };
      # Started by the test once both accounts exist: it signs in to each.
      systemd.services.mail-state.serviceConfig.ExecStart = "${pkgs.lib.getExe mailState} 8790 ${ca}/ca.pem andrew@example.com roundcube@example.com";
    };
    testScript =
      builtins.readFile ../record.py
      + builtins.readFile ./scenarios.py
      + builtins.readFile ./differential.py
      + mailu.setup
      + ''
        run_differential()
      '';
  })
