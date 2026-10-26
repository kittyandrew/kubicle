# kubicle as a user meets it, for the browser suites: Dovecot shaped like Mailu front's internal IMAP (STARTTLS
# required, PLAIN and LOGIN only), kubicle servers on it, and Caddy standing in for front (prefix stripped, Mailu's
# identity headers added on sso.php only). One NixOS module per VM node, which runs one or more browser sessions.
#   sessions: one kubicle server package per session. Session i has its own user `browser<i>`, wrapper
#     /run/current-system/sw/bin/browser<i>, kubicle (127.0.0.1:<9000+i>) and front
#     (http://localhost:<8087+i>/login/), and its own logs: /var/log/dnsmasq<i>.log, /var/log/sink<i>.log,
#     /var/log/caddy/front<i>.log and kernel log lines prefixed `kubicle-leak<i> `, so a leak names its session.
#   seed: shell run once before Dovecot first starts, with $MAIL the Maildir++ root of andrew@example.test.
#   browser: the browser package the wrappers run.
{
  browser,
  web,
  sessions,
  seed,
}: {
  lib,
  options,
  pkgs,
  ...
}: let
  tls = pkgs.callPackage ./vm/ca.nix {}; # the VM checks' CA, whose certificate also names imap.example.test
  numbered = lib.imap1 (i: server: {inherit i server;}) sessions;
  front = i: 8087 + i;
  sinkPort = i: 4000 + i;
  dnsPort = i: 5300 + i;
  # Every name resolves (TEST-NET-2), so the connection attempt shows too. The browsers' own update and telemetry
  # hosts get a second address, so the suites can tell their noise from a page's requests.
  dnsAnswers = ["--address=/#/198.51.100.1" "--address=/google.com/googleapis.com/gvt1.com/mozilla.com/mozilla.net/mozilla.org/198.51.100.2"];
  dovecotConf = pkgs.writeText "dovecot.conf" ''
    dovecot_config_version = 2.4.0
    dovecot_storage_version = 2.4.0
    protocols = imap
    listen = 127.0.0.1
    log_path = /dev/stderr
    base_dir = /run/dovecot
    state_dir = /var/lib/dovecot
    default_internal_user = dovecot
    default_internal_group = dovecot
    default_login_user = dovenull
    mail_driver = maildir
    mail_path = /var/lib/mail/%{user}
    mailbox_list_index = yes
    namespace inbox {
      inbox = yes
      separator = .
      mailbox Trash {
        auto = subscribe
        special_use = \Trash
      }
      mailbox Drafts {
        auto = subscribe
        special_use = \Drafts
      }
      mailbox Sent {
        auto = subscribe
        special_use = \Sent
      }
      mailbox Junk {
        auto = subscribe
        special_use = \Junk
      }
      mailbox Archive {
        auto = subscribe
        special_use = \Archive
      }
    }
    auth_mechanisms = plain login
    ssl = required
    ssl_server_cert_file = ${tls}/server.pem
    ssl_server_key_file = ${tls}/server.key
    passdb passwd-file {
      passwd_file_path = ${pkgs.writeText "users" "andrew@example.test:{PLAIN}fixture-password\n"}
    }
    userdb static {
      fields {
        uid = vmail
        gid = vmail
        home = /var/lib/mail/%{user}
      }
    }
    service imap-login {
      inet_listener imap {
        port = 10143
      }
    }
  '';
in {
  imports = [./capture.nix];
  # Guest memory freed after a heavy step goes back to the host: a test VM's host RSS fell from 2.6 GB to 0.6 GB
  # within 20 s of a 2 GB allocation ending, and stayed at 2.6 GB without this device. Only a VM has the option.
  virtualisation = lib.optionalAttrs (options ? virtualisation.qemu.options) {
    qemu.options = ["-device virtio-balloon-pci,free-page-reporting=on"];
  };
  # Session i's browser runs as `browser<i>`, which reaches its own front and nothing else: its DNS queries go to
  # its own dnsmasq, its other TCP connections to its own sink (sink.py), and any other packet to the kernel log,
  # refused. glibc must resolve in the browser's own process for the firewall to tell sessions apart, so no nscd.
  users.users =
    lib.listToAttrs (map ({i, ...}:
      lib.nameValuePair "browser${toString i}" {
        isSystemUser = true;
        group = "browser";
        home = "/var/lib/browser${toString i}";
        createHome = true;
      })
    numbered)
    // lib.genAttrs ["vmail" "dovecot" "dovenull"] (name: {
      isSystemUser = true;
      group = name;
    });
  users.groups.browser = {};
  services.nscd.enable = false;
  system.nssModules = lib.mkForce [];
  # Drivers run as root and make the profile, so the wrapper hands it to the session's user before dropping to it.
  environment.systemPackages = map ({i, ...}: let
    user = "browser${toString i}";
  in
    pkgs.writeShellScriptBin user ''
      prev=
      for a in "$@"; do
        case $a in --user-data-dir=*) chown -R ${user}: "''${a#*=}" ;; esac
        [ "$prev" = -profile ] && chown -R ${user}: "$a"
        prev=$a
      done
      # Chromium's network log, each request's events and net:: error, one file per launch in the user's home.
      exec ${pkgs.util-linux}/bin/setpriv --reuid=${user} --regid=browser --init-groups --reset-env -- ${lib.getExe browser} \
        ${lib.optionalString (lib.getName browser == "chromium") "--log-net-log=/var/lib/${user}/netlog-$$.json"} "$@"
    '')
  numbered;
  # Name resolution for everything else in the VM, unlogged: the server's image proxy resolves names too.
  services.dnsmasq = {
    enable = true;
    settings = {
      listen-address = "127.0.0.1";
      bind-interfaces = true;
      no-resolv = true;
      no-hosts = true;
      address = ["/#/198.51.100.1"];
    };
  };
  systemd.tmpfiles.rules =
    ["d /var/lib/mail 0755 vmail vmail -" "d /var/log/caddy 0755 caddy caddy -"]
    ++ lib.concatMap ({
      i,
      server,
    }: [
      # Session i's kubicle runs the build these link to; hostile's plant runs relink them and restart it.
      "L+ /run/kubicle${toString i}/server - - - - ${server}"
      "L+ /run/kubicle${toString i}/web - - - - ${web}"
      "f /var/log/dnsmasq${toString i}.log 0644 dnsmasq dnsmasq -"
      "f /var/log/sink${toString i}.log 0644 root root -"
      "f /var/log/caddy/front${toString i}.log 0644 caddy caddy -"
    ])
    numbered;
  # A default route to a dummy device, so a packet to any address reaches the OUTPUT chain to be logged.
  boot.kernelModules = ["dummy"];
  # Chromium aborts every request in flight on a network change. dhcpcd made two, 12-18 s after boot: sink's
  # link-local address once DHCP timed out ("Failed to fetch" on the hostile sign-ins), and QEMU's unused eth0
  # lease mid-load ("Could not load INBOX: network error" in explore; Oct 25, 2026).
  networking.dhcpcd.denyInterfaces = ["sink" "eth0"];
  networking.localCommands = ''
    ip link add sink type dummy && ip link set sink up
    ip route add default dev sink && ip -6 route add default dev sink
    ip addr add 198.51.100.1/32 dev sink && ip -6 addr add 2001:db8::1/128 dev sink # where sink.py listens
  '';
  networking.firewall.extraCommands =
    lib.concatMapStrings ({i, ...}: let
      owner = "-m owner --uid-owner browser${toString i}";
      sink = toString (sinkPort i);
    in ''
      for ipt in iptables ip6tables; do
        $ipt -A OUTPUT ${owner} -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
        $ipt -A OUTPUT ${owner} -o lo -p tcp --dport ${toString (front i)} -j ACCEPT
      done
      for ipt in iptables ip6tables; do
        $ipt -t nat -A OUTPUT ${owner} -p udp --dport 53 -j REDIRECT --to-ports ${toString (dnsPort i)}
        $ipt -A OUTPUT ${owner} -o lo -p udp --dport ${toString (dnsPort i)} -j ACCEPT
      done
      iptables -t nat -A OUTPUT ${owner} -p tcp -m multiport ! --dports ${toString (front i)} \
        -j DNAT --to-destination 198.51.100.1:${sink}
      iptables -A OUTPUT ${owner} -p tcp -d 198.51.100.1 --dport ${sink} -j ACCEPT
      ip6tables -t nat -A OUTPUT ${owner} -p tcp -m multiport ! --dports ${toString (front i)} \
        -j DNAT --to-destination [2001:db8::1]:${sink}
      ip6tables -A OUTPUT ${owner} -p tcp -d 2001:db8::1 --dport ${sink} -j ACCEPT
      for ipt in iptables ip6tables; do
        $ipt -A OUTPUT ${owner} -j LOG --log-prefix "kubicle-leak${toString i} "
        $ipt -A OUTPUT ${owner} -j REJECT
      done
    '')
    numbered;

  users.groups.vmail = {};
  users.groups.dovecot = {};
  users.groups.dovenull = {};

  systemd.services =
    {
      dovecot = {
        wantedBy = ["multi-user.target"];
        path = [pkgs.gnutar pkgs.zstd pkgs.coreutils]; # for the explore suite's mailbox tarball
        preStart = ''
          if [ ! -d /var/lib/mail/andrew@example.test ]; then
            MAIL=/var/lib/mail/andrew@example.test
            ${seed}
            chown -R vmail:vmail /var/lib/mail
          fi
        '';
        serviceConfig = {
          ExecStart = "${pkgs.dovecot}/bin/dovecot -F -c ${dovecotConf}";
          TimeoutStartSec = "5min"; # the explore suite's tarball unpacks 58,000 mails
          StateDirectory = "dovecot"; # not mail: systemd would chown it back to root before every command
        };
      };
    }
    // lib.listToAttrs (lib.concatMap ({
        i,
        server,
      }: let
        n = toString i;
      in [
        (lib.nameValuePair "sink${n}" {
          wantedBy = ["multi-user.target"];
          serviceConfig = {
            ExecStart = "${pkgs.python3}/bin/python3 -I ${./sink.py} ${toString (sinkPort i)} ${tls}/server.pem ${tls}/server.key";
            StandardOutput = "append:/var/log/sink${n}.log";
            DynamicUser = true;
          };
        })
        (lib.nameValuePair "dnsmasq${n}" {
          wantedBy = ["multi-user.target"];
          serviceConfig.ExecStart = lib.concatStringsSep " " ([
              "${pkgs.dnsmasq}/bin/dnsmasq --keep-in-foreground --user=dnsmasq --pid-file= --port=${toString (dnsPort i)}"
              "--listen-address=127.0.0.1,::1 --bind-interfaces --no-resolv --no-hosts --log-queries"
              "--log-facility=/var/log/dnsmasq${n}.log"
            ]
            ++ dnsAnswers);
        })
        (lib.nameValuePair "kubicle${n}" {
          wantedBy = ["multi-user.target"];
          after = ["dovecot.service"];
          environment = {
            KUBICLE_LISTEN = "127.0.0.1:${toString (9000 + i)}";
            KUBICLE_TRUSTED_PROXY = "127.0.0.1/32"; # this VM's Caddy
            KUBICLE_WEB_DIR = "/run/kubicle${n}/web";
            KUBICLE_DATA_DIR = "/var/lib/kubicle${n}";
            KUBICLE_PUBLIC_ORIGIN = "http://localhost:${toString (front i)}";
            KUBICLE_MAIL_HOST = "127.0.0.1";
            KUBICLE_MAIL_TLS_NAME = "imap.example.test";
            KUBICLE_MAIL_EXTRA_CA = "${tls}/ca.pem";
            KUBICLE_MAIL_DOMAINS = "example.test";
            KUBICLE_TIME_ZONE = "Europe/Kyiv";
            RUST_LOG = "info,kubicle_server=debug"; # a line per request, for the evidence (tests/record.py)
          };
          serviceConfig = {
            ExecStart = "/run/kubicle${n}/server/bin/${server.meta.mainProgram}";
            SyslogLevel = "debug"; # its lines stay off the console (capture.nix)
            DynamicUser = true;
            StateDirectory = "kubicle${n}";
          };
        })
      ])
      numbered);

  services.caddy = {
    enable = true;
    globalConfig = ''
      admin off
      auto_https off
    '';
    # JSON access logs keep request headers: the hostile suite reads each request's Sec-Fetch-Site there.
    extraConfig =
      lib.concatMapStrings ({i, ...}: let
        upstream = "127.0.0.1:${toString (9000 + i)}";
      in ''
        http://localhost:${toString (front i)} {
          log {
            output file /var/log/caddy/front${toString i}.log
            format json
          }
          handle /login/sso.php {
            uri strip_prefix /login
            reverse_proxy ${upstream} {
              header_up X-Remote-User andrew@example.test
              header_up X-Remote-User-Token fixture-password
            }
          }
          redir /sso/logout /login/sso.php # Mailu's sign-out, then its sign-in, which comes straight back here
          handle_path /login/* {
            reverse_proxy ${upstream} {
              header_up -X-Remote-User
              header_up -X-Remote-User-Token
            }
          }
        }
      '')
      numbered;
  };
}
