<?php
// vm-settings' way into Roundcube's settings (tests/vm/differential.py): stores one user's preferences and the
// fields of their one identity through Roundcube's own rcube_user, as bin/moduserprefs.sh and Settings' Save do.
// The values arrive typed as program/actions/settings/prefs_save.php would store them.
//
//   php roundcube-prefs.php ADDRESS '{"prefs": {...}, "identity": {"name": ..., "reply-to": ...}}'
define('INSTALL_PATH', '/var/www/roundcube/');
require_once INSTALL_PATH . 'program/include/clisetup.php';

[, $address, $json] = $argv;
$input = json_decode($json, true, 16, JSON_THROW_ON_ERROR);
$db = $rcmail->get_dbh();
$db->query('SELECT * FROM ' . $db->table_name('users', true) . ' WHERE `username` = ?', $address);
$row = $db->fetch_assoc();
if (!$row) {
    fwrite(STDERR, "no Roundcube user $address: it is created at its first sign-in\n");
    exit(1);
}
$user = new rcube_user($row['user_id'], $row);
if (!$user->save_prefs($input['prefs'] + $user->get_prefs(), true)) {
    fwrite(STDERR, "saving the preferences failed\n");
    exit(1);
}
$identities = $user->list_identities();
if (count($identities) != 1 || !$user->update_identity($identities[0]['identity_id'], $input['identity'])) {
    fwrite(STDERR, 'updating the identity failed: ' . count($identities) . " identities\n");
    exit(1);
}
// What a request now reads, for the test's log.
$stored = new rcube_user($row['user_id']);
echo json_encode(['prefs' => $stored->get_prefs(), 'identity' => $stored->list_identities()[0]]), "\n";
