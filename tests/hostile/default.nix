# The hostile-HTML suite (docs/design.md, Testing): every case of every corpus, mailed to a real mailbox, opens in
# the real client in a real browser. Four VMs: Chromium and Firefox, each against a sanitizing server and one that
# does not, which proves the sandbox alone. driver.py says what fails a case.
{
  lib,
  pkgs,
  server,
  web,
  # Cases rendered at once on the batch page (driver.py). 100 against 200 cut Chromium's peak from 2.96 to
  # 2.33 GB, and the full corpus ran 10% faster (Oct 25, 2026).
  batch ? 100,
  # Sessions per VM, its vCPUs and memory, per browser. Chromium is CPU-bound rendering the frames (host load 12.7
  # through the full corpus, Oct 25, 2026); Firefox mostly waits on the fetches. Memory is the guest's peak
  # `free -m` used over that run plus about 600 MB: Chromium 2.33 GB (sampled every 2 s through a full run, Oct 25,
  # 2026), Firefox 1.4 GB (at batches of 200).
  layout ? {
    chromium = {
      n = 4;
      cores = 4;
      memory = 2944;
    };
    firefox = {
      n = 2;
      cores = 2;
      memory = 2048;
    };
  },
}: let
  judge = import ../judge.nix {inherit pkgs;};
  corpora = pkgs.callPackage ./corpora.nix {};
  cases = pkgs.runCommand "kubicle-hostile-cases" {nativeBuildInputs = [pkgs.nodejs pkgs.python3];} ''
    node ${./cases/js-corpora.mjs} ${corpora.dompurify} ${corpora.h5sc} ${corpora.ept3} > js.jsonl
    python3 -I ${./cases/cases.py} $out js.jsonl ${corpora.httpleaks} ${corpora.portswigger} ${corpora.wpt} \
      ${corpora.roundcube} ${corpora.mail-parser} ${corpora.mimekit} ${corpora.mime-torture} ${corpora.espoofer} \
      ${corpora.blns}
  '';
  driver = pkgs.writers.writePython3Bin "kubicle-hostile" {
    libraries = [pkgs.python3Packages.selenium];
    flakeIgnore = ["E501" "E203" "W503"]; # flake8's 79 columns, and two rules ruff's format breaks
  } (builtins.readFile ./driver.py);
  # The server with ammonia's pass in render.rs taken out, for the reader and a part's own page alike: the HTML goes
  # into the document as the mail has it.
  unsanitized = patched server ''
    substituteInPlace crates/kubicle-mail/src/render.rs \
      --replace-fail 'let clean = builder.clean(html).to_string();' 'let clean = html.to_owned();'
  '';
  patched = drv: patch: drv.overrideAttrs (old: {postPatch = (old.postPatch or "") + patch;});

  # The manifest's folders dealt out to n sessions, largest first, each to the session with the fewest cases.
  split = picked: n:
    pkgs.runCommand "kubicle-hostile-split-${toString n}" {nativeBuildInputs = [pkgs.python3];} ''
      python3 -I - ${picked} ${toString n} <<'EOF'
      import json, sys
      folders, n = json.load(open(sys.argv[1])), int(sys.argv[2])
      parts = [{} for _ in range(n)]
      for name, ids in sorted(folders.items(), key=lambda f: -len(list(filter(None, f[1])))):
          min(parts, key=lambda p: sum(len(list(filter(None, ids))) for ids in p.values()))[name] = ids
      for i, part in enumerate(parts, 1):
          json.dump(part, open(f"{i}.json", "w"))
      EOF
      mkdir $out && mv *.json $out/
    '';

  # The app plus the batch page, a blank page the server serves from the app origin with the app's CSP.
  webWithBatch = web:
    pkgs.runCommand "kubicle-web-with-batch-page" {} ''
      cp -r ${web} $out && chmod u+w $out
      cp ${./batch.html} $out/hostile-batch.html && cp ${./batch.js} $out/hostile-batch.js
    '';

  # One VM per browser and server, running `n` sessions side by side on its share of the folders.
  node = picked: browser: kubicle: mode: {
    n,
    cores,
    memory,
  }: {pkgs, ...}: let
    sessions = lib.range 1 n;
  in {
    imports = [
      (import ../stack.nix {
        browser = pkgs.${browser};
        web = webWithBatch web;
        sessions = map (_: kubicle) sessions;
        seed = "cp -r ${cases}/mail $MAIL && chmod -R u+w $MAIL";
      })
    ];
    virtualisation = {
      memorySize = memory;
      inherit cores;
      diskSize = 2048;
    };

    systemd.services = lib.listToAttrs (lib.concatMap (i: [
        (lib.nameValuePair "webdriver${toString i}" {
          wantedBy = ["multi-user.target"];
          serviceConfig.ExecStart =
            {
              chromium = "${pkgs.chromedriver}/bin/chromedriver --port=${toString (4443 + i)}";
              firefox = "${pkgs.geckodriver}/bin/geckodriver --port ${toString (4443 + i)}";
            }.${
              browser
            };
        })
        (lib.nameValuePair "hostile${toString i}" {
          after = ["webdriver${toString i}.service" "kubicle${toString i}.service" "caddy.service"];
          serviceConfig = {
            Type = "oneshot";
            ExecStart = "${driver}/bin/kubicle-hostile ${browser} ${mode} ${toString i} ${toString batch} ${cases}/reader.json ${split picked n}/${toString i}.json /var/lib/hostile${toString i}.json";
          };
        })
      ])
      sessions);
  };
  # One run over `picked`, a manifest: four VMs, Chromium and Firefox each against both servers.
  suite = name: picked:
    judge (pkgs.testers.runNixOSTest {
      name = "kubicle-hostile${name}";
      nodes = {
        chromium = node picked "chromium" server "strict" layout.chromium;
        chromiumraw = node picked "chromium" unsanitized "sandbox" layout.chromium;
        firefox = node picked "firefox" server "strict" layout.firefox;
        firefoxraw = node picked "firefox" unsanitized "sandbox" layout.firefox;
      };
      testScript =
        builtins.readFile ../record.py
        + ''
          from datetime import timedelta

          start_all()
          units = {}


          def start(m):
              m.wait_for_unit("multi-user.target")
              units[m] = m.succeed("systemctl list-unit-files --no-legend 'hostile*.service' | cut -d' ' -f1").split()
              if not units[m]:
                  return fail(f"{m.name} has no hostile session units")
              m.succeed("systemctl start --no-block " + " ".join(units[m]))


          def sessions(m):
              for unit in units.get(m, []):
                  m.wait_until_succeeds(f"systemctl show -P ActiveState {unit} | grep -qx -e inactive -e failed", timeout=timedelta(hours=1))
                  lines = m.succeed(f"journalctl -u {unit} -o cat").splitlines()
                  print("\n".join([l for l in lines if not l.startswith(("FAIL", "NOTE"))][-8:] + [l for l in lines if l.startswith(("FAIL", "NOTE"))][:300]))
                  ran = any("cases in batches of" in l for l in lines)  # the driver's last line: it ran to the end
                  if m.succeed(f"systemctl show -P Result {unit}").strip() != "success" or not ran:
                      fail(f"{m.name} {unit}: the FAIL lines in its journal name each case")


          for m in machines:
              guarded(f"{m.name} start", lambda m=m: start(m))
          for m in machines:
              guarded(f"{m.name} sessions", lambda m=m: sessions(m))
              collect(m, "/var/lib/hostile*.json", "/var/lib/browser*/netlog-*.json", "/var/log/caddy", "/var/log/dnsmasq*.log", "/var/log/sink*.log")
          record()
        '';
    });

  # The planted bugs (.claude/rules/002-dev.md), each with the VMs whose run it must fail. `.plants` boots one VM
  # set and runs the quick set against the clean build and then each plant, relinking kubicle's build in place
  # (../stack.nix), and passes only when every plant fails every run listed for it. A plant whose patch no
  # longer applies fails the build, which says to update the plant.
  plants = let
    allowScripts = ''
      substituteInPlace crates/kubicle-web/src/reader.rs --replace-fail 'sandbox="allow-popups' 'sandbox="allow-scripts allow-popups'
    '';
  in {
    allow-scripts = {
      web = allowScripts;
      nodes = ["chromium" "chromiumraw" "firefox" "firefoxraw"];
    };
    preconnect-kept = {
      server = ''
        substituteInPlace crates/kubicle-mail/src/render.rs --replace-fail '.add_tags(["style", "font", "big"])' \
          '.add_tags(["style", "font", "big", "link"]).add_tag_attributes("link", ["rel", "href"])'
      '';
      nodes = ["chromium" "firefox"];
    };
    img-src-opened = {
      server = ''
        substituteInPlace crates/kubicle-server/src/http.rs --replace-fail "img-src 'self' data:" "img-src * data:"
      '';
      nodes = ["firefoxraw"]; # Firefox's srcset race; Chromium has none
    };
    scripts-run = {
      server = ''
        substituteInPlace crates/kubicle-mail/src/render.rs \
          --replace-fail 'http-equiv=\"Content-Security-Policy\" content=' 'name=\"no-csp\" content='
        substituteInPlace crates/kubicle-server/src/http.rs \
          --replace-fail "script-src 'self' 'wasm-unsafe-eval'" "script-src 'self' 'wasm-unsafe-eval' 'unsafe-inline'"
      '';
      web = allowScripts;
      nodes = ["chromiumraw" "firefoxraw"];
    };
    # The print page, the mail as a page of its own: its header loses the CSP and its sandbox allows scripts, and its
    # document loses its own policy.
    print-scripts-run = {
      server = ''
        substituteInPlace crates/kubicle-mail/src/render.rs \
          --replace-fail 'http-equiv=\"Content-Security-Policy\" content=' 'name=\"no-csp\" content=' \
          --replace-fail 'format!("{}; sandbox", mail_csp(prefix))' 'String::from("sandbox allow-scripts")'
      '';
      nodes = ["chromiumraw" "firefoxraw"];
    };
  };

  plantRun = let
    bases = {
      chromium = {
        browser = "chromium";
        server = server;
        mode = "strict";
      };
      chromiumraw = {
        browser = "chromium";
        server = unsanitized;
        mode = "sandbox";
      };
      firefox = {
        browser = "firefox";
        server = server;
        mode = "strict";
      };
      firefoxraw = {
        browser = "firefox";
        server = unsanitized;
        mode = "sandbox";
      };
    };
    quick = "${cases}/quick.json";
    builds = name: base:
      [
        {
          plant = "clean";
          inherit (base) server;
          web = webWithBatch web;
        }
      ]
      ++ lib.mapAttrsToList (plant: p: {
        inherit plant;
        server = patched base.server (p.server or "");
        web = webWithBatch (patched web (p.web or ""));
      }) (lib.filterAttrs (_: p: lib.elem name p.nodes) plants);
  in
    judge (pkgs.testers.runNixOSTest {
      name = "kubicle-hostile-plants";
      nodes =
        lib.mapAttrs (name: base: {pkgs, ...}: {
          imports = [
            (node quick base.browser base.server base.mode {
              n = 1;
              cores = 2;
              memory = 1536; # one session: 0.9 GB used at most (free -m)
            })
          ];
          # One session per VM: relinking kubicle's build serves every plant from one boot.
          systemd.services.plants = {
            path = [pkgs.curl pkgs.coreutils pkgs.systemd];
            serviceConfig.Type = "oneshot";
            script = lib.concatMapStrings (b: ''
              ln -sfn ${b.server} /run/kubicle1/server && ln -sfn ${b.web} /run/kubicle1/web
              systemctl restart kubicle1
              until curl -sf http://127.0.0.1:9001/healthz >/dev/null; do sleep 0.2; done
              ${driver}/bin/kubicle-hostile ${base.browser} ${base.mode} 1 ${toString batch} ${cases}/reader.json \
                ${split quick 1}/1.json /var/lib/plant-${b.plant}.json || true
            '') (builds name base);
          };
        })
        bases;
      testScript =
        builtins.readFile ../record.py
        + ''
          import json
          from datetime import timedelta

          expected = json.loads(${builtins.toJSON (builtins.toJSON (lib.mapAttrs (_: p: p.nodes) plants))})
          start_all()


          def start(m):
              m.wait_for_unit("multi-user.target")
              m.succeed("systemctl start --no-block plants.service")


          def caught(m):
              m.wait_until_succeeds("systemctl show -P ActiveState plants | grep -qx -e inactive -e failed", timeout=timedelta(minutes=30))
              reports = {}
              for path in m.succeed("ls /var/lib/plant-*.json").split():
                  plant = path.removeprefix("/var/lib/plant-").removesuffix(".json")
                  reports[plant] = json.loads(m.succeed(f"cat {path}"))
              seen = lambda plant: {(f["case"], f["problem"].split(":")[0]) for f in reports[plant]["failures"]}
              if "clean" not in reports:
                  return fail(f"{m.name}: the clean run wrote no report")
              for plant in sorted(p for p, nodes in expected.items() if m.name in nodes):
                  new = seen(plant) - seen("clean") if plant in reports else set()
                  verdict = "caught" if new else ("MISSED" if plant in reports else "MISSED (no report: the run broke)")
                  print(f"{m.name:12} {plant:16} {verdict}: {sorted(new)[:3]}")
                  if not new:
                      fail(f"planted bug {plant} not caught on {m.name}")


          for m in machines:
              guarded(f"{m.name} start", lambda m=m: start(m))
          for m in machines:
              guarded(f"{m.name} plants", lambda m=m: caught(m))
              collect(m, "/var/lib/plant-*.json", "/var/lib/browser*/netlog-*.json", "/var/log/caddy")
          record()
        '';
    });
in
  # The quick set (cases.py) is the few-hundred-case check while editing: .#checks.x86_64-linux.hostile.quick.
  suite "" "${cases}/manifest.json"
  // {
    quick = suite "-quick" "${cases}/quick.json";
    plants = plantRun;
  }
