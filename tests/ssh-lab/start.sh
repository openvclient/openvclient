/usr/sbin/sshd -f /etc/ssh/lab-main.conf -E /tmp/main.log
/usr/sbin/sshd -f /etc/ssh/lab-pam.conf -E /tmp/pam.log
/usr/sbin/sshd -f /etc/ssh/lab-2step.conf -E /tmp/2step.log
/usr/sbin/sshd-otp -f /etc/ssh/lab-otp.conf -E /tmp/otp.log
/usr/sbin/sshd -f /etc/ssh/lab-legacy.conf -E /tmp/legacy.log
exec tail -F /tmp/main.log /tmp/pam.log /tmp/2step.log /tmp/otp.log /tmp/legacy.log
