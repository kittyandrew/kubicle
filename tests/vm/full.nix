# vm-full: the scenarios on seed.py's whole mailbox, through Mailu front. The speed budgets are not
# measured here: the VM ran every step two to five times slower (tests/budgets/run.sh, on the machine itself).
{
  pkgs,
  kubicle,
}: let
  judge = import ../judge.nix {inherit pkgs;};
  mailbox = pkgs.callPackage ./mailbox.nix {};
  mailu = import ./mailu.nix {inherit pkgs kubicle;} {
    mail = "${mailbox}/full.tar.zst";
    memorySize = 3072; # peak in use 2343 MB, 687 of it kubicle-server, at the 50 MB forward (Oct 25, 2026)
  };
in
  judge (pkgs.testers.runNixOSTest {
    name = "kubicle-vm-full";
    nodes.machine.imports = [mailu.node ./image-server.nix];
    testScript =
      builtins.readFile ../record.py
      + builtins.readFile ./scenarios.py
      + builtins.readFile ./cost.py
      + mailu.setup
      + ''
        run_tier(
          ("the image proxy's fetch, redirect and refusal", image_proxy),
          ("the image proxy fetches 8 at a time at most", image_proxy_bounded),
          ("a part too large to show downloads from the attachment list", too_large_part),
          ("a session ends with its own Mailu session, not another's", own_tokens),
          ("one Mailu session holds one kubicle session", one_session_per_token),
          ("Archive by sender lists the folders once", archive_lists_once),
          ("boot.js encodes the hash's folder", boot_hash_encoded),
          *cost_scenarios(),
          ("a search stops once its request ended", search_stops_with_its_request),
          ("a render that killed kubicle is not run again", render_marker),
          ("an address made anew finds nothing of the account before", address_made_anew),
        )
      '';
  })
