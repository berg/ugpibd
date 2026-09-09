/* Minimal FX2LP firmware: copy a slice of the I2C EEPROM into RAM.
 *
 * No USB code at all. With RENUM=0 the FX2 core services vendor request 0xA0
 * (RAM read/write) in hardware, so the host loads this, releases the 8051,
 * waits for the done flag, re-asserts reset and reads the buffer back.
 *
 * Layout, fixed so the host can poke it:
 *   0x0FFA  device address byte (0xA0 for a 24LC256 with A2..A0 low)
 *   0x0FFB  status: 0 running, 0xAA done, 0xE1/0xE2 no ACK, 0xE3 bus error
 *   0x0FFC  EEPROM start address, big-endian
 *   0x1000  buffer, CHUNK bytes
 */
#include <stdint.h>

#define I2CS  (*(__xdata volatile uint8_t *)0xE678)
#define I2DAT (*(__xdata volatile uint8_t *)0xE679)

#define bmSTART  0x80
#define bmSTOP   0x40
#define bmLASTRD 0x20
#define bmBERR   0x04
#define bmACK    0x02
#define bmDONE   0x01

#define CHUNK 0x2000

__xdata __at(0x0FFA) volatile uint8_t g_daddr;
__xdata __at(0x0FFB) volatile uint8_t g_status;
__xdata __at(0x0FFC) volatile uint8_t g_addr_hi;
__xdata __at(0x0FFD) volatile uint8_t g_addr_lo;
__xdata __at(0x1000) volatile uint8_t g_buf[CHUNK];

static uint8_t wait_done(void) {
    /* Bounded, so a missing or wedged EEPROM cannot hang the part. */
    uint16_t spin = 0;
    while (!(I2CS & bmDONE)) {
        if (++spin == 0) {
            return 0;
        }
    }
    return 1;
}

void main(void) {
    uint16_t i;
    uint8_t dummy;

    g_status = 0x00;

    I2CS = bmSTART;
    I2DAT = g_daddr;                 /* device address, write */
    if (!wait_done()) { g_status = 0xE3; goto stop; }
    if (!(I2CS & bmACK)) { g_status = 0xE1; goto stop; }

    I2DAT = g_addr_hi;
    if (!wait_done()) { g_status = 0xE3; goto stop; }
    I2DAT = g_addr_lo;
    if (!wait_done()) { g_status = 0xE3; goto stop; }

    I2CS = bmSTART;                  /* repeated start */
    I2DAT = g_daddr | 0x01;          /* device address, read */
    if (!wait_done()) { g_status = 0xE3; goto stop; }
    if (!(I2CS & bmACK)) { g_status = 0xE2; goto stop; }

    /* The first I2DAT read returns nothing useful; it starts byte 0. */
    dummy = I2DAT;
    (void)dummy;
    if (!wait_done()) { g_status = 0xE3; goto stop; }

    for (i = 0; i < CHUNK; i++) {
        /* LASTRD must be set before the transfer of the final byte begins,
         * and each read starts the following one — so arm it one early. */
        if (i == CHUNK - 1) {
            I2CS |= bmLASTRD;
        }
        g_buf[i] = I2DAT;
        if (!wait_done()) { g_status = 0xE3; goto stop; }
    }
    g_status = 0xAA;

stop:
    I2CS |= bmSTOP;
    for (;;) {
    }
}
