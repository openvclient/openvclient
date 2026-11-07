FROM ubuntu:24.04
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends openssh-server && rm -rf /var/lib/apt/lists/* && mkdir -p /run/sshd \
 && useradd -m -s /bin/bash inner && echo 'inner:innerpw' | chpasswd \
 && install -d -m 700 -o inner -g inner /home/inner/.ssh
COPY authorized_keys /home/inner/.ssh/authorized_keys
RUN chown inner:inner /home/inner/.ssh/authorized_keys && chmod 600 /home/inner/.ssh/authorized_keys && echo inner-host > /home/inner/where.txt && chown inner /home/inner/where.txt
CMD ["/usr/sbin/sshd", "-D", "-e"]
