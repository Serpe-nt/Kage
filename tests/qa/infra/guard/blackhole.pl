#!/usr/bin/perl
# QA round four, guard topic (F197): a "proxy" whose kernel drops every SYN.
# It listens on --port with a backlog of 0 and never accepts, then fills its
# own accept queue with two connections of its own, so that Linux drops any
# further SYN (tcp_abort_on_overflow is 0): to a client this address
# answers nothing, as a proxy behind a firewall that drops. Runs in the
# postgres image (perl, no packages), see blackhole.yaml.
#   perl blackhole.pl PORT [silent]
# With `silent` it accepts every connection instead and never answers
# (a proxy that takes the TCP connection and never answers the CONNECT).
use strict;
use warnings;
use Socket;
my $port = $ARGV[0] // 3128;
my $mode = $ARGV[1] // 'drop';
socket(my $server, PF_INET, SOCK_STREAM, getprotobyname('tcp')) or die "socket: $!";
setsockopt($server, SOL_SOCKET, SO_REUSEADDR, pack('l', 1)) or die "setsockopt: $!";
bind($server, sockaddr_in($port, INADDR_ANY)) or die "bind: $!";
listen($server, 0) or die "listen: $!";
$| = 1;
if ($mode eq 'silent') {
    print "silent on $port\n";
    my @kept;
    while (1) {
        my $paddr = accept(my $client, $server);
        push @kept, $client if $paddr;
        shift @kept while @kept > 500;
    }
}
my @fill;
for my $i (1 .. 3) {
    socket(my $c, PF_INET, SOCK_STREAM, getprotobyname('tcp')) or die;
    # Non-blocking connect: the third one waits for a queue slot forever.
    my $flags = 0;
    use Fcntl;
    fcntl($c, F_SETFL, O_NONBLOCK);
    connect($c, sockaddr_in($port, inet_aton('127.0.0.1')));
    push @fill, $c;
}
print "dropping on $port\n";
sleep 1 while 1;
