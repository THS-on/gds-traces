KDIR  ?= /home/user/git/linux
CFLAGS = -O2 -Wall -Wextra -std=c11

all: module client

module:
	$(MAKE) -C $(KDIR) M=$(CURDIR) modules

client: relay_repro_client.c
	$(CC) $(CFLAGS) -o relay_repro_client relay_repro_client.c

clean:
	$(MAKE) -C $(KDIR) M=$(CURDIR) clean
	rm -f relay_repro_client

.PHONY: all module client clean
