FROM ubuntu:24.04
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends openssh-server libpam-google-authenticator && rm -rf /var/lib/apt/lists/* && mkdir -p /run/sshd
COPY keys/authorized_keys keys/user_ca.pub /lab/
COPY setup.sh start.sh /lab/
RUN bash /lab/setup.sh
EXPOSE 22 2223 2224 2225 2226
CMD ["bash", "/lab/start.sh"]
