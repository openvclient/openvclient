set -e
mk() { useradd -m -s /bin/bash "$1"; echo "$1:$2" | chpasswd; }
mk pwuser pwpass
mk keyuser keypw
mk certuser certpw
mk otpuser otppw
mk pamuser pampw
mk sftponly sftppw
echo 'sftp only' > /home/sftponly/hello.txt && chown sftponly /home/sftponly/hello.txt
ln -s /home/pwuser/nested /home/pwuser/link-to-dir
for u in keyuser; do install -d -m 700 -o $u -g $u /home/$u/.ssh; install -m 600 -o $u -g $u /lab/authorized_keys /home/$u/.ssh/authorized_keys; done
cp /lab/user_ca.pub /etc/ssh/user_ca.pub
# Files with awkward names for SFTP tests.
H=/home/pwuser
echo "unicode ok" > "$H/résumé ✓.txt"
echo "spaces ok" > "$H/file with spaces.txt"
echo "SECRET=1" > "$H/.env"
mkdir -p "$H/nested/deeper" && echo deep > "$H/nested/deeper/leaf.txt"
ln -s nested/deeper/leaf.txt "$H/link-to-leaf"
mkdir "$H/no-access" && chmod 000 "$H/no-access"
head -c 20971520 /dev/urandom > "$H/big.bin"
chown -R pwuser:pwuser "$H"; chown -h pwuser:pwuser "$H/link-to-dir" 2>/dev/null || true
# TOTP user: fixed secret plus emergency scratch codes.
printf 'JBSWY3DPEHPK3PXP\n" RATE_LIMIT 100 30\n" WINDOW_SIZE 17\n" TOTP_AUTH\n11111111\n22222222\n33333333\n44444444\n55555555\n' > /home/otpuser/.google_authenticator
chown otpuser:otpuser /home/otpuser/.google_authenticator && chmod 400 /home/otpuser/.google_authenticator
# Separate PAM service for the OTP instance (PAM service = program name).
ln -s /usr/sbin/sshd /usr/sbin/sshd-otp
printf '@include common-auth\nauth required pam_google_authenticator.so\n@include common-account\n@include common-session\n' > /etc/pam.d/sshd-otp
base='UsePAM yes
AllowTcpForwarding yes
Subsystem sftp internal-sftp
PrintMotd no
'
printf '%sPort 22\nPasswordAuthentication yes\nKbdInteractiveAuthentication yes\nPubkeyAuthentication yes\nTrustedUserCAKeys /etc/ssh/user_ca.pub\nMatch User sftponly\n  ForceCommand internal-sftp\n  AllowTcpForwarding no\n' "$base" > /etc/ssh/lab-main.conf
printf '%sPort 2223\nPasswordAuthentication no\nKbdInteractiveAuthentication yes\nAuthenticationMethods keyboard-interactive\n' "$base" > /etc/ssh/lab-pam.conf
printf '%sPort 2224\nPasswordAuthentication yes\nPubkeyAuthentication yes\nAuthenticationMethods publickey,password\n' "$base" > /etc/ssh/lab-2step.conf
printf '%sPort 2225\nPasswordAuthentication no\nKbdInteractiveAuthentication yes\nAuthenticationMethods keyboard-interactive\n' "$base" > /etc/ssh/lab-otp.conf
printf '%sPort 2226\nPasswordAuthentication yes\nHostKey /etc/ssh/ssh_host_rsa_key\nHostKeyAlgorithms ssh-rsa\nKexAlgorithms diffie-hellman-group14-sha1,diffie-hellman-group14-sha256\nCiphers aes128-ctr\nMACs hmac-sha1\n' "$base" > /etc/ssh/lab-legacy.conf
ssh-keygen -A
