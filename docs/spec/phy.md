# PHY and link programming: Intel 82599, X540, X552 (ixgbe family)

An independent specification for a polling, boot-time (UEFI) driver. It covers
how software reaches each PHY and how it brings the link up and reads it
back. It does not describe the driver in this repository. Its author did not
read that driver and did not write any of it.

- **Scope:** the Physical Functions of the 82599 (10 GbE controller), the X540
  (integrated 10GBASE-T) and the X552, which is the 10 GbE MAC inside the Xeon D-1500
  (the shared code calls it `X550EM_x`). The X553 (`X550EM_a`, Atom C3000), the
  X550, the 82598 and the E610 are out of scope. Where they share a mechanism,
  the text says so only to prevent confusion.
- **Sources:** Intel's ixgbe shared code under its BSD-3-Clause license
  (FreeBSD `sys/dev/ixgbe/`, commit `669cd0d90ee7`, 2026-09-29). The files
  used are `ixgbe_phy.c`, `ixgbe_phy.h`, `ixgbe_82599.c`, `ixgbe_x540.c`,
  `ixgbe_x550.c`, `ixgbe_common.c`, `ixgbe_api.c` and `ixgbe_type.h`, plus the
  BSD-licensed FreeBSD glue in `if_ix.c` (call order only) and `if_ix_mdio_hw.c`
  (clause-22 framing). The DPDK copy of the same base code
  (`drivers/net/intel/ixgbe/base/`, BSD-3-Clause) was used to confirm that the
  X552 Marvell path has no further programming. No GPL-only source was
  consulted.
- **Datasheets:** the public Intel datasheets (82599 10 GbE Controller, Ethernet
  Controller X540, Xeon D-1500 Vol. 4) could not be fetched from this
  environment: intel.com answered HTTP 403. Every register offset, field and
  sequence below therefore comes from the BSD shared code. Semantic
  descriptions ("means", "is") state what the shared code does with a field.
  Where the code gives no meaning, the text says so. Section 10 lists what
  should be checked against the datasheets or on hardware.
- **No code is reproduced.** Sequences are written as numbered steps and
  tables. Every section ends with a **Source** line (file and function) so a
  reader can check it.
- **Addendum:** section 11 was added on 2026-10-06 (#16) by the driver's
  maintainers, not by this document's author, from the same shared code at
  a later commit. Sections 1.2, 4.4, 4.5 and 10 point to it.

## Conventions

- Register offsets are byte offsets into BAR0 (memory-mapped CSR space).
  All CSRs are 32-bit, little-endian.
- Bits are numbered from 0 (LSB). "Bit 31" is `0x8000_0000`.
- "RMW" is read-modify-write: read the register, change only the named bits,
  write it back.
- "Flush" is a read of any CSR (the shared code reads STATUS, `0x00008`)
  after a write, so the write reaches the device before a delay starts.
- MDIO registers are written `D.R`: MMD (device) `D`, register `R`, both
  hexadecimal. For example `7.0x0020` is AN MMD register 0x20.
- `lan_id` is the port number from `STATUS.LAN_ID` (section 1.3). It is **not**
  the swapped PCI function number.
- Times are the shared code's own delays. "Up to N×T" means N polls with a
  delay of T between them.

---

## 1. Common infrastructure

### 1.1 CSRs used by PHY and link code

| Register | Offset | Devices | Used for |
|---|---|---|---|
| CTRL | `0x00000` | all | MAC reset: `RST` bit 26, `LNK_RST` bit 3 |
| STATUS | `0x00008` | all | `LAN_ID` bits 3:2; flush reads |
| ESDP | `0x00020` | all | Software-definable pins: laser, rate select, module presence, I2C mux |
| I2CCTL | `0x00028` | 82599, X540 | Bit-banged I2C to the SFP+ cage |
| I2CCTL | `0x15F5C` | X552 | Bit-banged I2C (different bit layout, section 3.1) |
| HLREG0 | `0x04240` | all | `MDCSPD` bit 16 (MDIO clock speed) |
| MSCA | `0x0425C` | all | MDIO command/address |
| MSRWD | `0x04260` | all | MDIO data |
| AUTOC | `0x042A0` | 82599 | Link mode and auto-negotiation control |
| LINKS | `0x042A4` | all | Link status and speed |
| AUTOC2 | `0x042A8` | 82599 | 10G serial PMA/PMD select, link disable |
| ANLP1 | `0x042B0` | 82599 | AN state (pipeline-reset handshake) |
| MMNGC | `0x042D0` | 82599, X540, X552 | `MNG_VETO` bit 0 |
| LINKS2 | `0x04324` | 82599+ | not needed for link-up |
| CORECTL | `0x14F00` | 82599 | Analog core register access; SFP init sequence target |
| MANC | `0x05820` | all | `RCV_TCO_EN` bit 17 (manageability receive enabled) |
| SWSM | `0x10140` | all | `SMBI` bit 0, `SWESMBI` bit 1 |
| FWSM | `0x10148` | all | Firmware mode bits 3:1 |
| FACTPS | `0x10150` | all | `LFS` bit 30 (port swap), `MNGCG` bit 29 |
| GSSR / SW_FW_SYNC | `0x10160` | all | SW/FW resource semaphores (section 1.4) |
| SB_IOSF_INDIRECT_CTRL | `0x11144` | X552 | IOSF sideband command |
| SB_IOSF_INDIRECT_DATA | `0x11148` | X552 | IOSF sideband data |
| FUSES0_GROUP(0) | `0x11158` | X552 | Silicon revision bits 7:6 (only for the LPLU choice) |
| NW_MNG_IF_SEL | `0x11178` | X552 | Board-strapped PHY interface selection (section 7.6) |

The X553 moves SWSM, FWSM, SW_FW_SYNC and FACTPS to `0x15Fxx`. That does not
apply to the X552, which uses the addresses above.

**Source:** `ixgbe_type.h` (register defines and the `_BY_MAC` tables).

### 1.2 PCI device IDs: MAC type, media and PHY path

Vendor `0x8086`. The shared code keys everything off the device ID: MAC type,
media type and the PHY path. It does not use the subsystem ID for link
decisions.

| Device ID | Shared-code name | MAC | Media (per the shared code) | PHY / link path |
|---|---|---|---|---|
| `0x10F7` | 82599_KX4 | 82599 | backplane | MAC PCS/PMA only. AUTOC KX4/KX (section 5) |
| `0x1514` | 82599_KX4_MEZZ | 82599 | backplane | as above |
| `0x1517` | 82599_KR | 82599 | backplane | AUTOC KR/KX4/KX AN |
| `0x10F8` | 82599_COMBO_BACKPLANE | 82599 | backplane | AUTOC KR/KX4/KX AN |
| `0x152A` | 82599_BACKPLANE_FCOE | 82599 | backplane | AUTOC |
| `0x10FC` | 82599_XAUI_LOM | 82599 | backplane | AUTOC (XAUI to an on-board PHY) |
| `0x10F9` | 82599_CX4 | 82599 | CX4 | AUTOC 10G parallel |
| `0x10FB` | 82599_SFP | 82599 | fiber (SFP+) | I2C module ID + NVM init sequence + AUTOC SFI |
| `0x1507` | 82599_SFP_EM | 82599 | fiber | as 0x10FB |
| `0x1529` | 82599_SFP_FCOE | 82599 | fiber | as 0x10FB |
| `0x154A` | 82599_SFP_SF_QP | 82599 | fiber | as 0x10FB |
| `0x154D` | 82599_SFP_SF2 | 82599 | fiber | as 0x10FB |
| `0x1557` | 82599EN_SFP | 82599 | fiber | as 0x10FB |
| `0x1558` | 82599_QSFP_SF_QP | 82599 | fiber, QSFP+ | QSFP ID on an I2C bus shared by ports (section 3.5) |
| `0x154F` | 82599_LS | 82599 | fiber "LCO" | no module ID, laser or rate select: AUTOC from the NVM, as for a backplane (section 11.1; missing from the shared code's MAC-type table) |
| `0x155D` | 82599_BYPASS | 82599 | fiber, fixed | always multispeed, soft rate select |
| `0x151C` | 82599_T3_LOM | 82599 | copper | external TN1010 10GBASE-T PHY on MDIO |
| `0x1528` | X540T | X540 | copper | integrated 10GBASE-T PHY on MDIO (section 6) |
| `0x1560` | X540T1 | X540 | copper | as 0x1528 (single port) |
| `0x155C` | X540_BYPASS | X540 | copper | as 0x1528 |
| `0x15AA` | X550EM_X_KX4 | X552 | backplane | internal KX4, run by hardware. No software PHY setup |
| `0x15AB` | X550EM_X_KR | X552 | backplane | internal KR PHY over IOSF sideband, KR/KX AN (section 7.4) |
| `0x15AC` | X550EM_X_SFP | X552 | fiber | internal KR PHY + CS4227 retimer on I2C + SFP+ (section 7.7) |
| `0x15AD` | X550EM_X_10G_T | X552 | copper | external X557 PHY on MDIO + internal iXFI/KR link (section 7.8) |
| `0x15AE` | X550EM_X_1G_T | X552 | copper | external Marvell 1G PHY, run by firmware (section 7.9) |
| `0x15B0` | X550EM_X_XFI | X552 | backplane | internal XFI, run by hardware (section 7.10) |

That is 26 Physical Function IDs in total. The Virtual Functions of the same
families are `0x10ED`, `0x152E` (82599), `0x1515`, `0x1530` (X540) and `0x15A8`,
`0x15A9` (X552). They have no PHY access and a boot driver must not bind them.

The X553 IDs (`0x15C2`–`0x15CE`, `0x15E4`, `0x15E5`) look similar but use a
different register block and firmware-token PHY access. They are out of scope.

**Source:** `ixgbe_type.h` (`IXGBE_DEV_ID_*`); `ixgbe_api.c`
`ixgbe_set_mac_type`; `ixgbe_82599.c` `ixgbe_get_media_type_82599`;
`ixgbe_x540.c` `ixgbe_get_media_type_X540`; `ixgbe_x550.c`
`ixgbe_get_media_type_X550em`, `ixgbe_identify_phy_x550em`.

### 1.3 Port number (lan_id)

- `lan_id` = `STATUS[3:2]` (mask `0x0000000C`, shift 2). On these parts it is 0
  or 1.
- `FACTPS.LFS` (bit 30) set means the PCI functions are swapped. The shared
  code then XORs the *function* number with 1 but leaves `lan_id` unchanged.
- Every per-port PHY decision uses `lan_id`: PHY0/PHY1 semaphore choice, the
  X552 KR register bank, the CS4227 slice, the SFP `core0`/`core1` type and
  the X552 I2C mux.

**Source:** `ixgbe_common.c` `ixgbe_set_lan_id_multi_port_pcie`.

### 1.4 Software/firmware semaphores

Firmware (the manageability engine, and on the X540/X552 the PHY firmware
path) shares the PHY, MDIO, I2C, NVM and some MAC CSRs with software. Access is
arbitrated in two layers:

1. **Register semaphore**, which guards the semaphore register itself.
2. **Resource bits** in GSSR / SW_FW_SYNC (`0x10160`): one software-owned bit
   and one firmware-owned bit per resource.

#### 1.4.1 Resource bits (`0x10160`)

| Bit | Mask | Name | Owner | Meaning |
|---|---|---|---|---|
| 0 | `0x0001` | EEP_SM | SW | NVM |
| 1 | `0x0002` | PHY0_SM | SW | PHY / MDIO of port 0 (X552: also the IOSF sideband, section 7.2) |
| 2 | `0x0004` | PHY1_SM | SW | PHY / MDIO of port 1 |
| 3 | `0x0008` | MAC_CSR_SM | SW | Shared MAC CSRs (82599: AUTOC when LESM is on; the CORECTL init sequence) |
| 4 | `0x0010` | FLASH_SM | SW (82599) / HW (X540, X552) | Flash. On the X540/X552 it is checked as a hardware-busy bit whenever EEP_SM is requested |
| 5–9 | `SW << 5` | FW_* | FW | Firmware owns the matching resource (bit 5 EEP … bit 8 MAC_CSR) |
| 9 | `0x0200` | NVM_UPDATE_SM | — | not used for link |
| 10 | `0x0400` | SW_MNG_SM | SW (X540+) | software-only, no firmware pair |
| 11 | `0x0800` | SW I2C0 | SW (X540+) | I2C bus of port 0 |
| 12 | `0x1000` | SW I2C1 | SW (X540+) | I2C bus of port 1 |
| 13, 14 | `I2C << 2` | FW I2C0/1 | FW | firmware owns the matching I2C bus |
| 30 | `0x4000_0000` | TOKEN_SM | — | X553 only |
| 31 | `0x8000_0000` | REGSMP | HW | X540/X552 register semaphore (section 1.4.3) |

Combined mask used by the X552 SFP path: `SHARED_I2C_SM = 0x1806` (PHY0, PHY1,
I2C0, I2C1).

#### 1.4.2 82599: acquire and release

Register semaphore (`SWSM`, `0x10140`):

1. Read SWSM up to 2000 times, 50 µs apart (100 ms). A read that returns
   `SMBI` (bit 0) clear grants SMBI to the reader; hardware sets the bit on
   that read. If it never reads clear: clear SMBI and SWESMBI, wait 50 µs,
   read once more, and succeed only if that read shows SMBI clear.
2. Then up to 2000 times, 50 µs apart: RMW-set `SWESMBI` (bit 1) and read it
   back. The semaphore is held once the read-back shows it set. On timeout,
   clear both bits and fail.
3. Release: RMW-clear `SWESMBI` and `SMBI` together, then flush.

Resource acquire, for mask `m` (SW bits) with firmware bits `m << 5`:

1. Up to 200 attempts, 5 ms apart (1 s): take the register semaphore, read
   GSSR, and if none of `m | (m << 5)` is set, write GSSR with `m` added. Then
   release the register semaphore; the resource is held. If any bit was set,
   release the register semaphore and wait 5 ms.
2. On timeout the shared code **clears whatever of `m | (m << 5)` is still
   set, firmware bits included**, waits 5 ms and returns failure. The caller
   decides whether to retry.

Resource release: take the register semaphore, clear the SW bits `m` in GSSR,
release the register semaphore.

#### 1.4.3 X540 and X552: acquire and release

Register semaphore: first `SWSM.SMBI` as in 1.4.2 step 1 (2000 × 50 µs, but
with no forced-clear retry). Then poll `SW_FW_SYNC` (`0x10160`) up to 2000
times, 50 µs apart, until a read returns `REGSMP` (bit 31) clear. As with
SMBI, the read that sees it clear grants it. On timeout, release both and
fail. Release: RMW-clear REGSMP in SW_FW_SYNC, then RMW-clear SMBI in SWSM,
then flush.

Resource acquire for a requested mask `m`:

- SW bits = `m & 0x000F`, plus `0x0400` if `m` includes SW_MNG, plus
  `m & 0x1800` (the I2C bits).
- FW bits = `(m & 0x000F) << 5`, plus `(m & 0x1800) << 2`.
- HW bits = `0x0010` (FLASH) if `m` includes EEP_SM, otherwise none.
- Timeout count: 200 on the X540 and **1000 on the X552** (every MAC type from
  the X550 on), each attempt 5 ms apart (1 s / 5 s).

1. Each attempt: take the register semaphore. If none of SW|FW|HW bits is
   set, write SW_FW_SYNC with the SW bits added and release the register
   semaphore; the resource is held. Otherwise release and wait 5 ms.
2. After the last attempt the shared code takes the register semaphore once
   more:
   - If FW or HW bits are still set, it **assumes the firmware has failed**:
     it sets the SW bits anyway, releases, waits 5 ms and returns *success*.
   - Otherwise, if SW bits are set (another software instance holds them), it
     clears all SW bits (EEP, PHY0, PHY1, MAC_CSR, SW_MNG, plus I2C if
     requested) and returns failure.
3. Release: take the register semaphore, clear the SW bits (`m & 0x040F` plus
   `m & 0x1800`), release the register semaphore. Then wait **10 µs** if `m`
   includes PHY0, PHY1 or SW_MNG, otherwise **2 ms**.

On the X552, acquiring any mask that includes an I2C bit also drives the
I2C mux for port 1 (section 7.7.1), and release undoes it.

**Start-up clean-up (X540, X552):** at attach the shared code takes and
releases the register semaphore once, ignoring the result. It then acquires
and releases EEP | PHY0 | PHY1 | MAC_CSR | SW_MNG | I2C0 | I2C1 through the
full algorithm above, which clears bits left behind by a crashed owner.

**Recommendation for a boot driver.** The forced take in step 2 exists for a
crashed operating-system driver. At boot, the likely holders are the
manageability firmware and the previous boot agent. A boot driver may
reasonably treat an acquire timeout as "do not touch the PHY; report link
from LINKS only" rather than forcing. Either way the timeouts above are the
reference values. This is a policy choice, not something the source requires.

**Source:** `ixgbe_common.c` `ixgbe_get_eeprom_semaphore`,
`ixgbe_release_eeprom_semaphore`, `ixgbe_acquire_swfw_sync`,
`ixgbe_release_swfw_sync`; `ixgbe_x540.c` `ixgbe_acquire_swfw_sync_X540`,
`ixgbe_release_swfw_sync_X540`, `ixgbe_get_swfw_sync_semaphore`,
`ixgbe_release_swfw_sync_semaphore`, `ixgbe_init_swfw_sync_X540`;
`ixgbe_x550.c` `ixgbe_acquire_swfw_sync_X550em`,
`ixgbe_release_swfw_sync_X550em`; `if_ix.c` attach (`ixgbe_init_swfw_semaphore`).

#### 1.4.4 Which semaphore protects which access

| Access | 82599 | X540 | X552 |
|---|---|---|---|
| MDIO to the port's PHY | PHY0 or PHY1 by lan_id | PHY0 or PHY1 by lan_id | 10G_T: PHY0 or PHY1 by lan_id |
| SFP I2C (module EEPROM) | PHY0 or PHY1 by lan_id | — | SFP: `0x1806` (both PHY + both I2C) + mux |
| CS4227 (I2C combined) | — | — | `0x1806` + mux |
| IOSF sideband (KR PHY) | — | — | PHY0 **and** PHY1 (`0x0006`), both ports' bits together |
| AUTOC write / pipeline reset | MAC_CSR, only if LESM is enabled (5.3) | — | — |
| CORECTL SFP init sequence | MAC_CSR | — | — |
| CTRL.RST / LNK_RST | none | the port's PHY semaphore | the port's `phy_semaphore_mask` (may be 0 for KR/KX4/XFI) |

`phy_semaphore_mask` is chosen as follows. By default it is PHY0 or PHY1 by
lan_id; generic PHY identification sets it if nothing else has. The X552 SFP
device overrides it to `0x1806`. On the X552 KR, KX4, XFI and 1G_T devices
the shared code never sets it, so it stays 0 and the MAC reset takes no
resource bit.

**Source:** `ixgbe_phy.c` `ixgbe_identify_phy_generic`,
`ixgbe_read_phy_reg_generic`, `ixgbe_write_phy_reg_generic`,
`ixgbe_read_i2c_byte_generic_int`; `ixgbe_82599.c` `prot_autoc_read_82599`,
`prot_autoc_write_82599`, `ixgbe_setup_sfp_modules_82599`; `ixgbe_x540.c`
`ixgbe_reset_hw_X540`; `ixgbe_x550.c` `ixgbe_init_phy_ops_X550em`,
`ixgbe_write_iosf_sb_reg_x550`, `ixgbe_read_iosf_sb_reg_x550`,
`ixgbe_reset_hw_X550em`.

### 1.5 Manageability: when not to touch the link

| Check | Register | Rule in the shared code |
|---|---|---|
| Link veto | `MMNGC` (`0x042D0`) bit 0 `MNG_VETO` | If set: do **not** reset the PHY, restart PHY AN, write AUTOC (82599), set up the X552 KR PHY, or drop the SFP+ laser. |
| MNG present | `FWSM` (`0x10148`) bits 3:1 == `010b` (value `0x4`, pass-through mode) | Used to avoid powering the copper PHY down. |
| MNG enabled | FWSM mode == pass-through, **and** `MANC` bit 17 (`RCV_TCO_EN`) set, **and** on 82599/X540 `FACTPS` bit 29 (`MNGCG`) clear | 82599: when set, the laser-control functions are disabled and the LMS-restore rule of section 5.13 applies. |

A boot driver should read MMNGC once before any disruptive step. If the veto is
set, it should leave the PHY alone and only report link from LINKS.

**Source:** `ixgbe_phy.c` `ixgbe_check_reset_blocked`; `ixgbe_common.c`
`ixgbe_mng_present`, `ixgbe_mng_enabled`.

---

## 2. MDIO access

### 2.1 MSCA (`0x0425C`) and MSRWD (`0x04260`)

| MSCA bits | Mask | Field | Meaning |
|---|---|---|---|
| 15:0 | `0x0000_FFFF` | NP_ADDR | Clause 45: the 16-bit register address (used in the address cycle) |
| 20:16 | `0x001F_0000` | DEV_TYPE | Clause 45: MMD number. Clause 22: the 5-bit register number |
| 25:21 | `0x03E0_0000` | PHY_ADDR | PHY (port) address 0–31 |
| 27:26 | `0x0C00_0000` | OP_CODE | `00` address cycle, `01` write, `11` read (C45), `10` read / read-increment (the C22 read op) |
| 29:28 | `0x3000_0000` | ST_CODE | `00` clause 45 ("new protocol"), `01` clause 22 ("old protocol") |
| 30 | `0x4000_0000` | MDI_COMMAND | Write 1 to start. Hardware clears it when the cycle completes |
| 31 | `0x8000_0000` | MDI_IN_PROG_EN | Not used by the shared code (written as 0) |

| MSRWD bits | Field |
|---|---|
| 15:0 | Write data (loaded before a write cycle) |
| 31:16 | Read data (valid after a read cycle completes) |

**Completion:** after every command write, poll MSCA every 10 µs, up to 100
polls (1 ms), for `MDI_COMMAND` to read 0. If it is still set, the access
fails: no PHY, or the bus is hung.

### 2.2 Clause 45 read and write

Read of `D.R` at PHY address `P`:

1. Write MSCA = `R` (bits 15:0) | `D` << 16 | `P` << 21 | op `00` |
   `MDI_COMMAND`. This is the address cycle, ST = `00`.
2. Poll for completion (2.1).
3. Write MSCA = the same address fields | op `11` (read) | `MDI_COMMAND`.
4. Poll for completion.
5. Read MSRWD. The data is bits 31:16.

Write of value `V`:

1. Write MSRWD = `V` (bits 15:0).
2. Address cycle as in read step 1, and poll.
3. Write MSCA = the same fields | op `01` (write) | `MDI_COMMAND`, and poll.

Each access is done while holding the port's PHY semaphore (section 1.4.4).
The shared code takes and releases it per register, not per sequence.

### 2.3 Clause 22 framing

All PHYs in scope are clause 45. Only FreeBSD's MDIO-bus glue uses clause 22,
and only for X553 devices. For completeness, a clause-22 access is **one**
command with no address cycle:

- Read: MSCA = register (5 bits) in bits 20:16 | PHY address in bits 25:21 |
  ST `01` (`0x1000_0000`) | op `10` (`0x0800_0000`) | `MDI_COMMAND`. Poll as
  in 2.1, then take MSRWD bits 31:16.
- Write: MSRWD = data. MSCA = the same fields with op `01` (`0x0400_0000`) |
  `MDI_COMMAND`. Poll.

### 2.4 MDIO clock (HLREG0.MDCSPD)

`HLREG0` (`0x04240`) bit 16 `MDCSPD` selects the MDC rate. The shared code
**clears** it (the slower rate) on the X552 10G_T before the first MDIO access,
and clears it again after the MAC reset. It does not touch the bit on the
82599 or X540, so they keep the value that hardware or the NVM loaded. (The
X553 1G_T parts set it; that is not relevant here.)

**Source:** `ixgbe_type.h` (MSCA/MSRWD fields); `ixgbe_phy.c`
`ixgbe_read_phy_reg_mdi`, `ixgbe_write_phy_reg_mdi`,
`ixgbe_read_phy_reg_generic`, `ixgbe_write_phy_reg_generic`;
`if_ix_mdio_hw.c` `ixgbe_read_mdio_unlocked_c22`,
`ixgbe_write_mdio_unlocked_c22`; `ixgbe_x550.c` `ixgbe_set_mdio_speed`.

### 2.5 PHY identification

**Validating an address.** Address `P` has a PHY if a read of `1.0x0002`
(PMA/PMD PHY ID high) returns something other than `0x0000` or `0xFFFF`.

**ID value.** The ID is (`1.0x0002` << 16) | (`1.0x0003` & `0xFFF0`). The low
four bits of `1.0x0003` are the revision and are masked off before comparing.

| PHY ID | Part | Shared-code PHY type | Found on |
|---|---|---|---|
| `0x0154_0200` | X540 integrated PHY | `aq` | X540 (0x1528, 0x1560, 0x155C) |
| `0x0154_0240`, `0x0154_0250` | X557 | `x550em_ext_t` | X552 10G_T (0x15AD) |
| `0x00A1_9410` | TN1010 | `tn` | 82599 T3 LOM (0x151C) |
| `0x0141_0DD0` | Marvell 88E1500 | `ext_1g_t` | (listed; X552 1G_T is typed by device ID, not probed) |
| `0x0141_0EA0` | Marvell 88E1543 | `ext_1g_t` | as above |
| `0x0154_0220`, `0x0154_0221`, `0x0154_0223` | X550 PHY | `aq` | X550 (out of scope) |
| `0x0043_A400` | QT2022 | `qt` | 82598 (out of scope) |
| `0x0342_9050` | NL (Netlogic) | `nl` | 82598 (out of scope) |

**Unknown ID.** Read `1.0x000B` (PMA/PMD extended ability). If bit 2
(10GBASE-T, `0x0004`) or bit 5 (1000BASE-T, `0x0020`) is set, the PHY is an
unknown copper PHY (`cu_unknown`, treated like `tn` for media purposes).
Otherwise it is a generic PHY.

**Which addresses are probed.**

- 82599 and X540: addresses 0 to 31 in order; the first valid address wins.
  If none is valid, the PHY address is reset to 0 and the caller decides
  whether that is an error.
  - 82599: if no PHY is found and the media is not copper, it falls through to
    SFP/QSFP module identification (section 4). If the result is still unknown,
    the PHY type is `none`, which is normal for backplane and SFP parts.
  - 82599 copper (T3 LOM) with no PHY found is an error.
- X552 10G_T: when `NW_MNG_IF_SEL` (`0x11178`) is non-zero, **only** the
  address in bits 7:3 (`MDIO_PHY_ADD`) is probed. A miss is an error; there is
  no scan. When the register reads 0, the 0–31 scan above is used. (See
  section 10: the field is documented only for the X553.)
- X552 KR, KX4, XFI, 1G_T and SFP: the type is fixed by device ID and there
  is no MDIO probe.

A full scan with no PHY costs up to 32 × 2 MDIO cycles × 1 ms, about 64 ms in
the worst case. A boot driver can skip the scan on 82599 IDs whose media is not
copper. The shared code scans anyway and simply finds nothing.

**Source:** `ixgbe_phy.c` `ixgbe_validate_phy_addr`, `ixgbe_get_phy_id`,
`ixgbe_get_phy_type_from_id`, `ixgbe_probe_phy`, `ixgbe_identify_phy_generic`;
`ixgbe_82599.c` `ixgbe_identify_phy_82599`; `ixgbe_x550.c`
`ixgbe_identify_phy_x550em`, `ixgbe_read_mng_if_sel_x550em`.

### 2.6 Generic PHY reset

Used for the X557 and for 82599 external PHYs. It is **not** used for the
X540: the X540 PHY reset operation is absent, so software never soft-resets
the X540 PHY.

1. Skip the reset if `MMNGC.MNG_VETO` is set.
2. Skip it on 82599 T3 LOM if the PHY reports over-temperature. The alarm is
   `1.0x9005` (LASI status) bit 0 (`0x0001`). A caller may override this
   check.
3. Write `4.0x0000` (PHY XS control) = `0x8000` (bit 15, soft reset).
4. Up to 30 times, 100 ms apart (3 s):
   - X557: read `1.0xCC02` (vendor alarms 3). Done when bits 1:0 (`0x3`,
     "reset complete") are non-zero.
   - Other PHYs: read `4.0x0000`. Done when bit 15 has cleared.
   After done, wait about 2 µs.
5. If `4.0x0000` bit 15 is still set at the end, the reset failed.

**Source:** `ixgbe_phy.c` `ixgbe_reset_phy_generic`, `ixgbe_tn_check_overtemp`.

### 2.7 Clause 45 registers used in this document

| Reg | Name (per the shared code) | Bits used |
|---|---|---|
| `1.0x0000` | PMA/PMD control | — |
| `1.0x0002` / `1.0x0003` | PHY ID high / low | ID; low nibble of 0x0003 = revision |
| `1.0x0004` | Speed ability | bit 0 10G, bit 4 1G, bit 5 100M |
| `1.0x000B` | Extended ability | bit 2 10GBASE-T, bit 5 1000BASE-T, bit 7 100BASE-TX |
| `1.0x9005` | XENPAK LASI status | bit 0 (TN1010 temperature alarm) |
| `1.0xCC02` | Vendor alarms 3 (X557) | bits 1:0 PHY firmware reset complete |
| `4.0x0000` | PHY XS control | bit 15 soft reset |
| `7.0x0000` | AN control | bit 9 (`0x0200`) restart AN |
| `7.0x0001` | AN status | bit 2 (`0x0004`) link up (latching low: read twice); bit 5 AN complete |
| `7.0x0010` | AN advertisement | bit 8 100BASE-TX full, bit 7 100BASE-TX half |
| `7.0x0017` | AN XNP transmit | bit 14 1000BASE-T full (TN1010 only) |
| `7.0x0020` | 10GBASE-T AN control | bit 12 advertise 10GBASE-T |
| `7.0xC400` | AN vendor provisioning 1 | bit 15 advertise 1000BASE-T full (X540, X557); bit 10 2.5G, bit 11 5G (X550 only) |
| `7.0xC800` | AN vendor status | bits 2:0 resolved speed/duplex (table in 6.6) |
| `7.0xCC00` / `7.0xCC01` | AN vendor TX alarm / alarm 2 | 0xCC01 bit 0 link-state change |
| `7.0xD401` | PMA TX vendor LASI mask (X557) | bit 0 LASI enable |
| `30.0x0000` | Vendor-specific 1 control | bit 11 (`0x0800`) low-power mode |
| `30.0x0001` | Vendor-specific 1 status (TN1010) | bit 3 link, bit 4 1G (0 = 10G) |
| `30.0x000B` | TN1010 firmware revision | — |
| `30.0x0020` | X540/X557 PHY firmware revision | — |
| `30.0xC479` | Global reserved provisioning 10 (X557) | bit 15 power-up stall |
| `30.0xC850` | Global fault message | `0x8007` = high-temperature fault |
| `30.0xCC00` | Global alarm 1 | bit 4 device fault, bit 14 high-temp failure |
| `30.0xD400` | Global interrupt mask | bit 4 dev-fault enable, bit 14 high-temp enable |
| `30.0xFC00` / `30.0xFF00` | Chip standard interrupt flag / mask | bit 0 vendor alarm, bit 9 alarm 2 |
| `30.0xFC01` / `30.0xFF01` | Chip vendor interrupt flag / mask | bit 2 global alarm 1, bit 12 AN vendor alarm |

**Source:** `ixgbe_type.h` (`IXGBE_MDIO_*`, `IXGBE_MII_*`, `AQ_FW_REV`, `TNX_FW_REV`).

---

## 3. I2C to the SFP+ cage and the CS4227

The shared code bit-bangs I2C through I2CCTL. There is no I2C engine in use.

### 3.1 I2CCTL layouts

| Function | 82599 / X540 (`0x00028`) | X552 (`0x15F5C`) |
|---|---|---|
| SCL in (read the line) | bit 0 (`0x0001`) | bit 14 (`0x4000`) |
| SCL out | bit 1 (`0x0002`) | bit 9 (`0x0200`) |
| SDA in | bit 2 (`0x0004`) | bit 12 (`0x1000`) |
| SDA out | bit 3 (`0x0008`) | bit 10 (`0x0400`) |
| SDA output-enable, active low (1 = released) | — | bit 11 (`0x0800`) |
| SCL output-enable, active low (1 = released) | — | bit 13 (`0x2000`) |
| Bit-bang enable | — | bit 8 (`0x0100`) |

On the 82599/X540 a line is released by writing its "out" bit as 1 (open
drain). On the X552 the line also has to be released with its OE_N bit.

### 3.2 Bit-level primitives and timing

The timing values, in µs, are: rise 1, fall 1, data set-up 1, clock high 4,
clock low 5, start set-up 5, start hold 4, stop set-up 4, bus free 5.

- **Set SDA to `b`:** set or clear SDA-out and (X552) clear SDA-OE_N, which
  drives the line. Flush, then wait rise + fall + set-up (3 µs). If `b` = 1,
  (X552) set SDA-OE_N to release the line, re-read I2CCTL and check that
  SDA-in = 1. A mismatch is an error, because something is holding SDA low.
  With `b` = 0 there is no check.
- **Raise SCL:** (X552) set SCL-OE_N first. Then up to 500 times: set SCL-out,
  flush, wait 1 µs, and stop as soon as SCL-in reads 1. This is the
  clock-stretching allowance of 500 µs; the shared code does not fail when it
  expires.
- **Lower SCL:** clear SCL-out and (X552) clear SCL-OE_N. Flush, wait 1 µs.
- **Read SDA:** (X552) set SDA-OE_N to release, flush, wait 1 µs. Sample SDA-in.
- **Start:** (X552) set bit-bang enable. SDA = 1, raise SCL, wait 5, SDA = 0,
  wait 4, lower SCL, wait 5.
- **Stop:** SDA = 0, raise SCL, wait 4, SDA = 1, wait 5. (X552) Then clear
  bit-bang enable and set both OE_N bits (release both lines), and flush.
- **Clock out one bit:** set SDA, raise SCL, wait 4, lower SCL, wait 5.
- **Clock in one bit:** (X552) release SDA. Raise SCL, wait 4, sample SDA,
  lower SCL, wait 5.
- **Clock out a byte:** eight bits, most significant first. Then release SDA:
  SDA-out = 1 and (X552) SDA-OE_N = 1.
- **Get ACK:** (X552) release SDA. Raise SCL, wait 4. Then sample SDA up to 10
  times, 1 µs apart. ACK means SDA reads 0. Then lower SCL and wait 5. No ACK
  is an error.
- **Bus clear**, used after any failed transfer: start, SDA = 1, nine SCL
  pulses (high 4, low 5), start, stop.

### 3.3 Byte transfers (SFF-8472 EEPROM, port expander)

Device addresses are 8-bit with the R/W bit in bit 0: `0xA0` for the SFP ID
EEPROM, `0xA2` for SFF-8472 diagnostics, `0xE0` for the port expander and
`0xBE` for the CS4227.

- **Read byte `off` from device `A`:** start; `A` (write) + ACK; `off` + ACK;
  repeated start; `A | 1` + ACK; clock in 8 bits; clock out a NACK (1); stop.
- **Write byte:** start; `A` + ACK; `off` + ACK; data + ACK; stop.

Retries and locking, from `ixgbe_read_i2c_byte_generic_int` and
`ixgbe_write_i2c_byte_generic_int`:

| | 82599 / X540 | X552 |
|---|---|---|
| Read attempts | 11 (10 retries) | 4 (3 retries) |
| Read attempts when probing the identifier byte (`A0:0`) while no module is known | 11 | 11 |
| Semaphore (locked variants) | taken per attempt | taken per attempt |
| After a failed read attempt | bus clear, release the semaphore, **wait 100 ms** | same |
| Write attempts | 2 (1 retry), semaphore held across both | same |

The "unlocked" variants do the same without the semaphore. The caller holds
it, which is how the CS4227 reset sequence (7.7.2) runs.

### 3.4 Which I2C bus, and its semaphore

- **82599:** each port has its own I2CCTL. The shared code takes the port's
  PHY0/PHY1 semaphore around each byte.
- **X552 SFP (0x15AC):** both ports share one I2C segment, which carries both
  cages, the CS4227 and the port expander. The segment is behind a mux
  controlled by ESDP SDP1 of port 1. The semaphore is `0x1806`, and acquiring
  it switches the mux (section 7.7.1).

### 3.5 82599 QSFP (0x1558): shared bus handshake

At PHY init, RMW ESDP: set `SDP0_DIR` (bit 8, SDP0 as output), clear `SDP1_DIR`
(bit 9, SDP1 as input), clear `SDP0` (bit 0), and clear `SDP0_NATIVE` (bit 16)
and `SDP1_NATIVE` (bit 17). Then, around every I2C byte:

1. Request the bus: set `SDP0`, flush.
2. Wait for the grant: poll `SDP1` = 1, up to 200 × 5 ms (1 s). On timeout,
   fail with an I2C error and still release.
3. Do the generic byte transfer (3.3).
4. Release: clear `SDP0`, flush.

### 3.6 I2C "combined" format (CS4227 16-bit registers)

The CS4227 has 16-bit register addresses and 16-bit data, wrapped with a
checksum.

The checksum is the 8-bit one's-complement sum of the given bytes (add; fold
the carry back in; keep 8 bits), then bitwise inverted.

**Read register `R` from device `A`** (`A` = `0xBE`):

1. `hi` = ((`R` >> 7) & `0xFE`) | 1, where bit 0 = 1 marks a read. `lo` = `R`
   & `0xFF`. `csum` = invert(ones-sum(`hi`, `lo`)).
2. Start; `A` + ACK; `hi` + ACK; `lo` + ACK; `csum` + ACK.
3. Repeated start; `A | 1` + ACK. Clock in the high data byte and send ACK
   (SDA 0). Clock in the low data byte and send ACK. Clock in the device's
   checksum byte and send a NACK. Stop.
4. Value = high << 8 | low. The shared code **does not verify** the returned
   checksum byte.
5. Retries: 4 attempts in total. After each failure: bus clear and release
   the semaphore; there is no 100 ms delay here.

**Write value `V` to register `R`:**

1. `hi` = (`R` >> 7) & `0xFE`, where bit 0 = 0 marks a write. `lo` = `R` &
   `0xFF`. `csum` = invert(ones-sum(`hi`, `lo`, `V` >> 8, `V` & `0xFF`)).
2. Start; `A`, `hi`, `lo`, `V` >> 8, `V` & `0xFF`, `csum`, each followed by
   ACK; stop.
3. Retries: 2 attempts in total.

Note that `hi` carries register bits 14:8 in its bits 7:1. The 16-bit register
address is therefore effectively 15 bits.

**Source:** `ixgbe_type.h` (I2CCTL fields per MAC); `ixgbe_phy.h` (timing,
addresses); `ixgbe_phy.c` `ixgbe_i2c_start`, `ixgbe_i2c_stop`,
`ixgbe_clock_in_i2c_bit`, `ixgbe_clock_out_i2c_bit`, `ixgbe_clock_out_i2c_byte`,
`ixgbe_get_i2c_ack`, `ixgbe_raise_i2c_clk`, `ixgbe_lower_i2c_clk`,
`ixgbe_set_i2c_data`, `ixgbe_get_i2c_data`, `ixgbe_i2c_bus_clear`,
`ixgbe_read_i2c_byte_generic_int`, `ixgbe_write_i2c_byte_generic_int`,
`ixgbe_read_i2c_combined_generic_int`, `ixgbe_write_i2c_combined_generic_int`;
`ixgbe_82599.c` `ixgbe_init_phy_ops_82599`, `ixgbe_read_i2c_byte_82599`,
`ixgbe_write_i2c_byte_82599`.

---

## 4. SFP+ module identification (SFF-8472)

This applies to the 82599 SFP devices and the X552 SFP (0x15AC).

### 4.1 Bytes read (device `0xA0` unless noted)

| Byte | Name | Values used |
|---|---|---|
| 0 | Identifier | `0x03` = SFP/SFP+. (`0x0D` = QSFP+, 82599 QSFP only) |
| 3 | 10G Ethernet compliance | bit 4 `0x10` 10GBASE-SR, bit 5 `0x20` 10GBASE-LR |
| 6 | 1G Ethernet compliance | bit 0 `0x01` 1000BASE-SX, bit 1 `0x02` LX, bit 3 `0x08` 1000BASE-T, bit 6 `0x40` BX10 |
| 8 | SFP+ cable technology | bit 2 `0x04` passive direct-attach, bit 3 `0x08` active direct-attach |
| 12 | Nominal bit rate (100 MBd units) | `0x67` (103, i.e. 10.3 GBd) marks 10G-BX |
| 14 | Single-mode length, km | > 0 counts toward 10G-BX |
| 15 | Single-mode length, 100 m units | ≥ 10 counts toward 10G-BX |
| 37–39 | Vendor OUI | combined as byte37 << 24 \| byte38 << 16 \| byte39 << 8 |
| 60 | Cable specification compliance (active DA) | bit 2 `0x04` limiting |
| `A2`: 110 (`0x6E`) | Status/control | bit 3 soft rate select RS0 |
| `A2`: 118 (`0x76`) | Extended status/control | bit 3 soft rate select RS1 |

Known OUIs: Intel `0x001B2100`, Tyco `0x00407600`, Finisar (FTL) `0x00906500`,
Avago `0x00176A00`.

### 4.2 Algorithm

1. Read the identifier up to 5 times. Stop early on an I2C error or on the
   value `0x03`. Some modules ACK before their microcontroller has data ready,
   which is why a successful read of a wrong value is repeated. An I2C failure
   means **no module present**.
2. Identifier not `0x03`: the module is unsupported.
3. Read bytes 6, 3 and 8. Any read failure means not present.
4. **10G-BX test:** byte 3 = 0, and byte 6 has none of SX, LX or 1000BASE-T,
   and byte 8 has neither DA bit. Then read byte 12; if it is `0x67`, read
   bytes 14 and 15. It is 10G-BX if byte 12 = `0x67` and (byte 14 > 0 or byte
   15 ≥ 10).
5. Classify. The first matching row wins. `N` = lan_id, which selects
   `core0` or `core1`:

| Order | Condition | sfp_type | Value (core0 / core1) |
|---|---|---|---|
| 1 | byte 8 bit 2 (passive DA) | `da_cu_coreN` | 3 / 4 |
| 2 | byte 8 bit 3 (active DA): read byte 60; bit 2 set → limiting | `da_act_lmt_coreN`, else `unknown` | 7 / 8 |
| 3 | byte 3 has SR or LR | `srlr_coreN` | 5 / 6 |
| 4 | byte 6 bit 3 (1000BASE-T) | `1g_cu_coreN` | 9 / 10 |
| 5 | byte 6 bit 0 (SX) | `1g_sx_coreN` | 11 / 12 |
| 6 | byte 6 bit 1 (LX) | `1g_lx_coreN` | 13 / 14 |
| 7 | 10G-BX (step 4) | `10g_bx_coreN` | 17 / 18 |
| 8 | byte 6 bit 6 (BX10) | `1g_bx_coreN` | 15 / 16 |
| — | nothing matched | `unknown` | `0xFFFF` |
| — | no module / I2C failure | `not_present` | `0xFFFE` |

   The numeric values matter on the 82599: they are the keys of the NVM
   init-sequence list (section 5.5).
6. If the type differs from the previously stored type, the module needs setup
   (`sfp_setup_needed`). On a first run the stored type is "unknown", so setup
   is always needed.
7. **Multispeed** (`multispeed_fiber`) is set when (SX and SR) or (LX and LR),
   or when the module is passive or active DA. It selects the try-10G-then-1G
   link algorithm (5.8).
8. **Vendor:** read bytes 37–39. The vendor only picks a PHY type name, which
   matters for the support rule below.
9. **Support rules**, in order:
   - Any DA cable (byte 8 bit 2 or 3) is **supported**, whatever the vendor.
   - Byte 3 = 0 and the type is not one of the 1G types (cu, sx, lx, bx) or
     10G-BX: **unsupported**.
   - If NVM word `0x2C` (Device Caps) bit 0 (`ALLOW_ANY_SFP`) is clear, and
     the type is not a 1G or 10G-BX type, only Intel-OUI modules are supported.
     Others are rejected unless the host allows unsupported modules; FreeBSD's
     default is not to.
   - Otherwise supported.
10. **X552 extra filter** (7.7.4): `1g_cu` and `unknown` are rejected even when
    the generic step accepts them.

**Boot-driver note.** The Intel-only rule is a support policy, not a hardware
limit. A boot driver may choose to accept any module that classifies as
SR/LR/DA/1G. It must still refuse `unknown`, because there is no NVM init
sequence (82599) or EDC mode (X552) for it.

### 4.3 Rate select

- **Hard (82599, `fiber` media):** ESDP `SDP5_DIR` (bit 13) = 1 (output).
  `SDP5` (bit 5) = 1 for 10G, 0 for 1G. Flush.
- **Soft (X552 SFP; 82599 bypass 0x155D):** RMW `A2:0x6E` bit 3 and `A2:0x76`
  bit 3, set for 10G and clear for 1G. Other bits are preserved. A failed read
  or write is logged and ignored.

### 4.4 Module presence

The shared code reads module presence directly only when the "crosstalk fix"
is active: NVM word `0x2C` bit 7 (`NO_CROSSTALK_WR`) is **clear**, on an 82599
or X552 with fiber media (on the 82599, SFP+ or QSFP+; section 11.4). The X552
reads that word through the firmware host interface (section 11.2). In that
case:

- 82599: cage full = ESDP `SDP2` (bit 2) set.
- X552: cage full = ESDP `SDP0` (bit 0) set.

With the fix active, link is reported down while the cage is empty. A LINKS
"up" is confirmed by reading LINKS again after 5 ms. The polarity is taken
as-is from the shared code (a set bit means full). A boot driver can use the
same bits as a cheap presence test before spending I2C retries.

**Source:** `ixgbe_phy.h` (SFF constants); `ixgbe_phy.c`
`ixgbe_identify_sfp_module_generic`, `ixgbe_identify_module_generic`,
`ixgbe_get_supported_phy_sfp_layer_generic`; `ixgbe_82599.c`
`ixgbe_set_hard_rate_select_speed`; `ixgbe_common.c`
`ixgbe_set_soft_rate_select_speed`, `ixgbe_check_mac_link_generic`,
`ixgbe_need_crosstalk_fix`, `ixgbe_start_hw_generic`; `if_ix.c` `ixgbe_handle_mod`.

### 4.5 QSFP+ (82599 0x1558)

The identifier must be `0x0D`. Byte `0x83` holds the 10G compliance (bit 3
`0x08` = passive DA, bit 0 `0x01` = active DA, bits 4 and 5 = SR and LR as for
SFP). Byte `0x86` holds the 1G compliance. Passive DA maps to `da_cu_coreN`;
SR/LR to `srlr_coreN`. Active DA maps to `da_act_lmt_coreN`; a module counts as
active DA if byte `0x83` bit 0 is set, or byte `0x82` = `0x23` and byte `0x92`
> 0 and (byte `0x93` >> 4) = 0. Anything else is unsupported. The OUI is at
`0xA5`–`0xA7`; the enforcement rule is as in 4.2 step 9. QSFP multispeed uses
no rate select, and QSFP never uses full auto-negotiation. Which QSFP modules
are multispeed is in section 11.3.

**Source:** `ixgbe_phy.c` `ixgbe_identify_qsfp_module_generic`.

---

## 5. 82599 link programming

The 82599 MAC has its own KX/KX4/KR/SFI PCS and PMA, configured through AUTOC
and AUTOC2. Only the T3 LOM has an external MDIO PHY.

### 5.1 AUTOC (`0x042A0`)

| Bits | Mask | Field | Values |
|---|---|---|---|
| 0 | `0x0000_0001` | FLU | Force link up |
| 8:7 | `0x0000_0180` | 10G_PMA_PMD (parallel) | `00` XAUI, `01` KX4, `10` CX4 |
| 9 | `0x0000_0200` | 1G_PMA_PMD | `0` BX / SFI, `1` KX |
| 12 | `0x0000_1000` | Restart_AN | set to restart auto-negotiation / apply LMS |
| 15:13 | `0x0000_E000` | LMS (link mode select) | see below |
| 16 | `0x0001_0000` | KR_SUPP | advertise / allow 10GBASE-KR |
| 17 | `0x0002_0000` | FECR | FEC requested |
| 18 | `0x0004_0000` | FECA | FEC ability (the header's AN_RX_ALIGN mask `0x007C_0000` overlaps this bit) |
| 22:18 | `0x007C_0000` | AN_RX_ALIGN | not touched |
| 23 | `0x0080_0000` | AN_RX_DRIFT | not touched |
| 24 | `0x0100_0000` | AN_RX_LOOSE | not touched |
| 26:25 | `0x0600_0000` | PD_TMR | not touched |
| 27 | `0x0800_0000` | RF | not touched |
| 28 | `0x1000_0000` | SYM_PAUSE | flow control advertisement |
| 29 | `0x2000_0000` | ASM_PAUSE | flow control advertisement |
| 30 | `0x4000_0000` | KX_SUPP | advertise 1000BASE-KX |
| 31 | `0x8000_0000` | KX4_SUPP | advertise 10GBASE-KX4 |

LMS (bits 15:13):

| LMS | Value in AUTOC | Mode | Speeds (5.6) | AN |
|---|---|---|---|---|
| `000` | `0x0000` | 1G link, no AN | 1G | no |
| `001` | `0x2000` | 10G parallel link, no AN (KX4 / CX4 / XAUI) | 10G | no |
| `010` | `0x4000` | 1G with clause-37 AN | 1G | yes |
| `011` | `0x6000` | 10G serial (KR or SFI per AUTOC2) | 10G | no |
| `100` | `0x8000` | KX4/KX/KR clause-73 AN | from KR/KX4/KX_SUPP | yes |
| `101` | `0xA000` | SGMII 1G/100M | 1G + 100M | no |
| `110` | `0xC000` | KX4/KX/KR AN + 1G AN | from the SUPP bits | yes |
| `111` | `0xE000` | KX4/KX/KR AN + SGMII | 100M + SUPP bits | yes |

### 5.2 AUTOC2 (`0x042A8`), LINKS, ANLP1, CORECTL

| Register | Bits | Field |
|---|---|---|
| AUTOC2 | 17:16 (`0x0003_0000`) | 10G serial PMA/PMD: `00` KR, `01` XFI, `10` SFI |
| AUTOC2 | 30:28 (`0x7000_0000`) | Link disable. The shared code clears all three whenever it resets the pipeline or the MAC |
| AUTOC2 | 30 and 28 (`0x5000_0000`) | Link disable in D3 (set only on a D3 transition; not for boot) |
| AUTOC2 | 31:16 (`0xFFFF_0000`) | "Upper" half, restored from the post-reset snapshot after later resets |
| ANLP1 (`0x042B0`) | 19:16 (`0x000F_0000`) | AN arbitration state; non-zero = AN has left state 0 |
| ANLP1 | 10, 11 | link partner sym / asym pause |
| CORECTL (`0x14F00`) | 15:0 | Analog register write: bits 15:8 = register, 7:0 = value. Bit 16 (`0x0001_0000`) = read command |

LINKS is described in section 8.

### 5.3 LESM and the MAC_CSR semaphore around AUTOC

Some NVM images run a firmware Link Establishment State Machine (LESM). When it
is enabled, every AUTOC write, and the pipeline reset that follows it, must
hold the **MAC_CSR** semaphore. The check is three NVM word reads (16-bit
words, word offsets):

1. `fw_ptr` = NVM[`0x0F`]. If it is 0 or `0xFFFF`: no LESM.
2. `lesm_ptr` = NVM[`fw_ptr` + `0x02`]. If it is 0 or `0xFFFF`: no LESM.
3. `state` = NVM[`lesm_ptr` + `0x01`]. LESM is enabled if bit 15 (`0x8000`)
   is set.

When LESM is enabled, the SmartSpeed algorithm (5.10) is not used.

### 5.4 Writing AUTOC: the "protected write" and pipeline reset

The shared code applies every AUTOC change the same way, as a protected write
followed by a pipeline reset:

1. If `MMNGC.MNG_VETO` is set, **write nothing**. The request is treated as
   successful.
2. If LESM is enabled (5.3), acquire MAC_CSR (section 1.4.2).
3. Write AUTOC with the new value.
4. Pipeline reset:
   1. If any AUTOC2 bit in 30:28 is set, clear those bits and flush.
   2. Read AUTOC and set Restart_AN (bit 12).
   3. Write AUTOC with **LMS bit 2 (register bit 15) inverted**, Restart_AN
      set.
   4. Poll ANLP1 every 4 ms, up to 10 times (40 ms), until bits 19:16 are
      non-zero.
   5. Always write AUTOC back with the original LMS and Restart_AN set, then
      flush.
   6. If the poll in step 4 never saw a non-zero state, return "reset failed".
5. Release MAC_CSR if it was taken.

The LMS toggle forces the MAC to re-run link setup even when the mode is
unchanged. A plain Restart_AN does not do this in the non-AN modes.

**Source:** `ixgbe_82599.c` `prot_autoc_read_82599`, `prot_autoc_write_82599`,
`ixgbe_reset_pipeline_82599`, `ixgbe_verify_lesm_fw_enabled_82599`.

### 5.5 SFP+ module setup: the NVM init sequence

This runs after identification (section 4), when the module is supported and
setup is needed. It is skipped when the type is `unknown`.

1. **Map the type** for the lookup: `da_act_lmt`, `1g_lx`, `1g_cu`, `1g_sx`,
   `1g_bx` and `10g_bx` are looked up as `srlr` of the same core (5 or 6).
   `da_cu` stays 3 or 4, and `srlr` stays 5 or 6.
2. **Find the list:** `list` = NVM[`0x2B`]. If it is 0 or `0xFFFF`, there is no
   init sequence and setup fails.
3. **Walk the list.** Starting at word `list + 1`, the list is pairs of (ID
   word, data-pointer word), ended by an ID of `0xFFFF`. Compare each ID with
   the mapped type. On a match, the next word is the data pointer `data`; a
   pointer of 0 or `0xFFFF` means the module is not supported. If the end
   marker is reached without a match, the module is not supported.
4. **Load the analog settings.** Acquire **MAC_CSR** (always, whether or not
   LESM is on). Skip the first word of the data block (word `data`). Then for
   `data + 1`, `data + 2`, …, while the word is not `0xFFFF`: write it to
   CORECTL and flush. Release MAC_CSR.
5. Wait 10 ms (the shared code's "semaphore delay"), so firmware can get the
   semaphore.
6. **Enter SFI:** protected write (5.4) of AUTOC = *original AUTOC* (5.13)
   OR `0x6000` (LMS `011`, 10G serial). This uses an OR, not a replace: an
   original LMS with bit 2 set would become `111`. For SFP SKUs the NVM
   default LMS is expected to be `000` or `011`; verify on hardware
   (section 10).

After a successful setup, the generic PHY reset is disabled for the port:
the SFP path has no PHY to reset.

**Source:** `ixgbe_phy.c` `ixgbe_get_sfp_init_sequence_offsets`;
`ixgbe_82599.c` `ixgbe_setup_sfp_modules_82599`, `ixgbe_init_phy_ops_82599`.

### 5.6 Link capabilities

The rules below are checked in order.

1. 1G module types (cu, lx, sx, bx): 1G, "autoneg" = true.
2. `da_cu`: 10G, plus 1G if multispeed; "autoneg" = true.
3. `10g_bx`: 10G only, no autoneg.
4. Otherwise, from the *original* AUTOC (the post-reset snapshot, 5.13), or the
   live AUTOC if there is no snapshot yet, by LMS:
   - `000` → 1G, no AN. `001` → 10G, no AN. `010` → 1G, AN. `011` → 10G, no AN.
   - `100`, `110` → 10G if KR_SUPP or KX4_SUPP; 1G if KX_SUPP; AN.
   - `111` → the same plus 100M; AN. `101` → 1G + 100M, no AN.
   - Any other value is an error.
   - If multispeed fiber: add 10G and 1G, and set autoneg = true (false for
     QSFP).

**Source:** `ixgbe_82599.c` `ixgbe_get_link_capabilities_82599`.

### 5.7 Setting the link speed (`setup_mac_link`)

Given the requested speeds `S`, masked by the capabilities of 5.6 (empty
means error):

1. Read AUTOC (current), AUTOC2 and the original AUTOC.
2. **AN modes (LMS `100`, `110`, `111`):** clear KX4_SUPP, KX_SUPP and
   KR_SUPP. If 10G is in `S`: set KX4_SUPP if the original had it, and set
   KR_SUPP if the original had it and SmartSpeed has not disabled KR (5.10).
   If 1G is in `S`: set KX_SUPP.
3. **1G SFI → 10G SFI:** if AUTOC bit 9 = 0 (SFI), LMS is `000` or `010`, `S`
   is exactly 10G and AUTOC2 bits 17:16 = `10` (SFI): set LMS = `011`.
4. **10G SFI → 1G SFI:** if AUTOC2 = SFI, LMS = `011`, `S` is exactly 1G and
   AUTOC bit 9 = 0: set LMS = `010` if autoneg (per 5.6) or the module is an
   Intel QSFP, otherwise `000`.
5. If the new AUTOC differs from the current value, do a protected write
   (5.4). Then, if the caller asked to wait and LMS is one of the AN modes,
   poll LINKS bit 31 (`KX_AN_COMP`) every 100 ms, up to 45 times (4.5 s).
   Timing out means "AN did not complete", which is reported but is not fatal.
   Finally wait **50 ms** so noise during initial link setup settles.
6. If AUTOC is unchanged, nothing is written and there is no wait.

**Source:** `ixgbe_82599.c` `ixgbe_setup_mac_link_82599`.

### 5.8 Multispeed fiber (SFP+ with multispeed set)

This is used as `setup_link` when multispeed is set. `S` is masked by the
capabilities.

1. If 10G is in `S`:
   1. Rate select 10G (4.3). QSFP: none.
   2. Wait 40 ms for the module to change its analog settings.
   3. `setup_mac_link(10G)` (5.7). Errors abort.
   4. Flap the laser if an "autotry restart" is pending (5.9).
   5. Poll link (section 8) every 100 ms, up to 10 times (1 s). If it is up:
      done.
2. If 1G is in `S`:
   1. Rate select 1G. Wait 40 ms.
   2. `setup_mac_link(1G)`. Flap the laser if pending.
   3. Wait 100 ms, then check link once. If it is up: done.
3. No link and more than one speed was tried: run the whole algorithm again
   with only the highest speed tried. This leaves the port at 10G, waiting.
4. Record the advertised speeds (10G and/or 1G from `S`).

A single-speed module uses `setup_mac_link` (5.7) directly.

**Source:** `ixgbe_common.c` `ixgbe_setup_mac_link_multispeed_fiber`;
`ixgbe_82599.c` `ixgbe_init_mac_link_ops_82599`.

### 5.9 Laser control (82599 SFP+)

This is active only for `fiber` media, and only when manageability is **not**
enabled (1.5). Otherwise the shared code never drives the laser.

- **Disable TX laser:** skip if `MNG_VETO`. Set ESDP `SDP3` (bit 3), flush,
  wait 100 µs. SDP3 = 1 drives the module's TX_DISABLE.
- **Enable TX laser:** clear ESDP `SDP3`, flush, wait **100 ms**.
- **Flap:** if an "autotry restart" is pending: disable then enable, and clear
  the pending flag. The pending flag is set after hardware start-up, and
  whenever the advertised speeds change. It tells the link partner to restart
  its own speed detection.
- FreeBSD enables the laser at attach and again at every link configuration,
  because it disables it on interface stop.

The shared code never programs `SDP3_DIR`. It relies on the NVM having made
SDP3 an output. A boot driver should read ESDP and check bit 11
(`SDP3_DIR`) = 1 before trusting the laser control (section 10).

**Source:** `ixgbe_82599.c` `ixgbe_disable_tx_laser_multispeed_fiber`,
`ixgbe_enable_tx_laser_multispeed_fiber`, `ixgbe_flap_tx_laser_multispeed_fiber`,
`ixgbe_start_hw_82599`; `if_ix.c` `ixgbe_config_link`.

### 5.10 SmartSpeed (82599 backplane)

This is used as `setup_link` for backplane media when SmartSpeed is enabled
(FreeBSD's default is "on") and LESM is **not** enabled. Its purpose is to
fall back from KR to KX4/KX when KR training fails on a marginal backplane.

1. Up to 3 rounds: `setup_mac_link(S)` (5.7) with KR allowed. Poll link every
   100 ms, up to 5 times (500 ms). If it is up: done.
2. If the original AUTOC advertises KR together with KX4 or KX: mark KR
   disabled and run `setup_mac_link(S)` again. Poll every 100 ms, up to 6
   times (600 ms). If it is up: done. (A 1G result is a "downgrade", which is
   only logged.)
3. Still down: mark KR allowed again and run `setup_mac_link(S)` a final time.

**Source:** `ixgbe_82599.c` `ixgbe_setup_mac_link_smartspeed`.

### 5.11 82599 with an external copper PHY (T3 LOM, TN1010)

1. The PHY is found by MDIO (2.5). Media is copper.
2. Capabilities come from `1.0x0004` (2.7): bit 0 10G, bit 4 1G, bit 5 100M.
3. Advertisement, for each speed the PHY supports:
   - 10G: `7.0x0020` bit 12.
   - 1G: `7.0x0017` bit 14. The TN1010 uses the XNP transmit register here,
     not `0xC400`.
   - 100M: `7.0x0010` bit 8.
   Then restart AN (`7.0x0000` bit 9) unless vetoed.
4. Then restart the MAC side: pipeline reset (5.4 step 4, under MAC_CSR if
   LESM) and wait 50 ms. If waiting was requested and LMS is an AN mode, poll
   `KX_AN_COMP` as in 5.7.
5. PHY link status: `30.0x0001`, sampled up to 10 times 10 µs apart. Bit 3 =
   link up; bit 4 = 1G (clear = 10G).
6. Over-temperature (2.6 step 2): the PHY is not reset while the alarm is set.

**Source:** `ixgbe_82599.c` `ixgbe_setup_copper_link_82599`,
`ixgbe_start_mac_link_82599`; `ixgbe_phy.c` `ixgbe_setup_phy_link_tnx`,
`ixgbe_check_phy_link_tnx`, `ixgbe_get_copper_speeds_supported`.

### 5.12 SFI firmware version check (informational)

For fiber media, the shared code reads `fw_ptr` = NVM[`0x0F`], then `ptp` =
NVM[`fw_ptr` + `0x04`] (pass-through patch configuration), then `ver` =
NVM[`ptp` + `0x07`]. It expects `ver` > 5. Missing pointers are not an error.
A low version returns an "NVM version" error from hardware start-up, which
FreeBSD treats as a warning. A boot driver may log the value and continue.

**Source:** `ixgbe_82599.c` `ixgbe_verify_fw_version_82599`.

### 5.13 MAC reset and the "original AUTOC"

The link-relevant parts of the 82599 MAC reset:

1. Before resetting, identify the PHY or module (sections 2.5 and 4) and run
   SFP setup if needed (5.5). An unsupported module aborts the reset.
2. Remember the current LMS.
3. Choose the reset type: `CTRL.LNK_RST` (bit 3) if the link is down, or
   `CTRL.RST` (bit 26) if the link is up. The shared code's comment explains
   that a link reset while the link is up could reset a PHY that
   manageability is using. Set the bit (RMW), flush, and poll for both bits
   to clear: 10 polls of 1 µs. Then wait **50 ms**. (If PCIe master disable
   failed earlier, the reset is done twice.)
4. After the reset, read AUTOC and AUTOC2. If any AUTOC2 bit in 30:28 is set,
   clear them.
5. **The first time:** store AUTOC and AUTOC2 as the "original" values, which
   are the NVM defaults.
   **Later times:** if (multispeed fiber and manageability enabled) or
   Wake-on-LAN is enabled, replace the original LMS with the LMS remembered in
   step 2. If AUTOC differs from the original, do a protected write (5.4) of
   the original. If AUTOC2 bits 31:16 differ from the original, restore those
   bits.

A boot driver that does its own MAC reset should keep the same rule: take the
snapshot after the first reset and never lose it. Every capability decision
(5.6, 5.7) reads the snapshot, not the live register.

**Source:** `ixgbe_82599.c` `ixgbe_reset_hw_82599`; `ixgbe_common.c`
`ixgbe_disable_pcie_primary`.

### 5.14 Module-reset quirks, in summary

- Some modules ACK I2C before their data is valid. Re-read the identifier (up
  to 5 successful reads) and use the long probe retry count (11 attempts, with
  100 ms between failed attempts).
- A module swapped for one of a different type requires the whole of 5.5
  again. The shared code tracks this with `sfp_setup_needed`.
- An unsupported module must stop link setup. The shared code refuses to
  finish the MAC reset and returns "SFP not supported".
- No module at start: the reset carries on and reports "not present". FreeBSD
  then polls for insertion. The FreeBSD insertion handler, when the crosstalk
  fix is active, first checks cage-full (4.4). It then identifies the module
  and runs module setup, then the link setup of 5.7/5.8 with the capabilities
  as the requested speeds.
- The MAC reset restores the NVM AUTOC (5.13), so module setup must be redone
  after any MAC reset that follows it. The shared code does module setup
  inside the reset for this reason.

**Source:** `ixgbe_phy.c` `ixgbe_identify_sfp_module_generic`;
`ixgbe_82599.c` `ixgbe_reset_hw_82599`; `if_ix.c` `ixgbe_handle_mod`,
`ixgbe_handle_msf`.

---

## 6. X540: integrated 10GBASE-T PHY

### 6.1 Overview

- Media is always copper. The MAC has no AUTOC path for these IDs. The link
  is the integrated PHY's copper auto-negotiation. Software reaches the PHY
  with clause-45 MDIO (section 2) under the port's PHY0/PHY1 semaphore, using
  the X540 algorithm of section 1.4.3.
- The PHY runs its own firmware. Software **does not reset the PHY**; the
  shared code sets the PHY reset operation to none on the X540. Software only
  advertises speeds, restarts AN, powers the PHY on or off, and reads status.
- MAC link status comes from LINKS (section 8).

### 6.2 Identification

The generic scan (2.5), addresses 0–31, finds ID `0x0154_0200` (type `aq`).
The semaphore mask is PHY0 or PHY1 by lan_id. Which MDIO address each port's
PHY answers at is not stated in the shared code; the scan takes the first
responder. See section 10.

### 6.3 Power

`30.0x0000` bit 11 (`0x0800`) is low-power mode. To power the PHY on, clear the
bit (RMW). FreeBSD does this at attach. Powering off (setting the bit) is
skipped when manageability is present (FWSM pass-through) or `MNG_VETO` is
set. A boot driver should power the PHY on unconditionally: a previous
operating system may have left it in low-power mode.

### 6.4 Capabilities

`1.0x0004`: bit 0 → 10G, bit 4 → 1G, bit 5 → 100M. The X540 does not
advertise 2.5G or 5G. The shared code reads this once and caches it.

### 6.5 Advertisement and AN restart

For the requested speeds `S` ∩ capabilities:

1. RMW `7.0x0020`: bit 12 = 1 if 10G is advertised, else 0.
2. RMW `7.0xC400`: bit 15 = 1 if 1G is advertised, else 0. (Bits 10 and 11,
   2.5G and 5G, are written only on the X550.)
3. RMW `7.0x0010`: clear bit 7 (100M half) and set bit 8 if 100M is
   advertised.
4. Unless `MMNGC.MNG_VETO` is set: RMW `7.0x0000`, set bit 9 (restart AN).

Each register access takes and releases the PHY semaphore on its own. The
shared code does not wait for AN completion inside this routine.

With no explicit request, the default is the full capability set from 6.4:
10G, 1G and 100M.

### 6.6 Status

- **MAC:** LINKS (section 8). This is what the shared code uses on the X540.
- **PHY (optional diagnostics):**
  - `7.0x0001` bit 2: link. It latches low, so read it twice back-to-back and
    use the second value. Bit 5: AN complete.
  - `7.0xC800` bits 2:0: resolved speed and duplex, per this table:

| `7.0xC800` [2:0] | Result |
|---|---|
| `000` | 10M half |
| `001` | 10M full |
| `010` | 100M half |
| `011` | 100M full |
| `100` | 1G half |
| `101` | 1G full |
| `110` | 10G half |
| `111` | 10G full |

  The table is the shared code's X557 decoding (7.8.6). That it applies to the
  X540 PHY is an inference from the shared AN vendor register block, not
  something the code does (section 10).
- PHY firmware revision: `30.0x0020`.

### 6.7 MAC reset (X540)

The shared code acquires the port's PHY semaphore, sets `CTRL.RST` (bit 26,
never `LNK_RST`), flushes and releases. It polls for `RST`/`LNK_RST` to clear
(10 × 1 µs), then waits **100 ms** (twice if a double reset is needed). The PHY
keeps running through a MAC reset.

**Source:** `ixgbe_x540.c` `ixgbe_init_ops_X540`, `ixgbe_get_media_type_X540`,
`ixgbe_setup_mac_link_X540`, `ixgbe_reset_hw_X540`; `ixgbe_phy.c`
`ixgbe_identify_phy_generic`, `ixgbe_get_copper_speeds_supported`,
`ixgbe_setup_phy_link_generic`, `ixgbe_setup_phy_link_speed_generic`,
`ixgbe_restart_auto_neg`, `ixgbe_set_copper_phy_power`,
`ixgbe_get_phy_firmware_version_generic`; `if_ix.c` attach.

---

## 7. X552 (Xeon D-1500, `X550EM_x`)

### 7.1 Per-device summary

| ID | Internal link | External part | What software programs | Link status |
|---|---|---|---|---|
| `0x15AA` KX4 | KX4 | backplane | nothing | LINKS |
| `0x15AB` KR | KR, clause-73 AN | backplane | KR PHY AN capabilities + restart (IOSF) | LINKS |
| `0x15AC` SFP | KR, AN toward the CS4227 host side | CS4227 retimer + SFP+ cage | I2C mux, CS4227 reset (once per power-on), module ID, KR PHY, CS4227 EDC mode | LINKS |
| `0x15AD` 10G_T | iXFI (forced) or KR, by `NW_MNG_IF_SEL` bit 24 | X557 10GBASE-T PHY on MDIO | X557 unstall, reset, advertisement; KR PHY forced iXFI speed follows the copper speed | LINKS **and** X557 `7.0x0001` bit 2 |
| `0x15AE` 1G_T | SGMII/1G (not programmed) | Marvell 1G PHY, managed by firmware | nothing | LINKS |
| `0x15B0` XFI | XFI | backplane / module | nothing | LINKS |

On every X552 part, the MAC-side KR PHY is reached through the **IOSF
sideband** (7.2), not MDIO. There is no AUTOC register.

**Source:** `ixgbe_x550.c` `ixgbe_init_ops_X550EM`, `ixgbe_init_ops_X550EM_x`,
`ixgbe_identify_phy_x550em`, `ixgbe_init_phy_ops_X550em`,
`ixgbe_init_mac_link_ops_X550em`, `ixgbe_get_link_capabilities_X550em`.

### 7.2 IOSF sideband interface

| Register | Offset | Bits | Field |
|---|---|---|---|
| SB_IOSF_INDIRECT_CTRL | `0x11144` | 15:0 | Target register address. The header's "ADDR_MASK" is `0xFF`, but the addresses used go up to `0x9A00` and are written unmasked into the low bits |
| | | 19:18 (`0x000C_0000`) | RESP_STAT: non-zero = the transaction failed |
| | | 27:20 (`0x0FF0_0000`) | CMPL_ERR: error code when RESP_STAT ≠ 0 |
| | | 30:28 | TARGET_SELECT: `0` = KR PHY (the only target used) |
| | | 31 (`0x8000_0000`) | BUSY: set while a transaction is in flight |
| SB_IOSF_INDIRECT_DATA | `0x11148` | 31:0 | Data (32-bit) |

**Read** register `a` of the KR PHY:

1. Acquire the SW_FW_SYNC bits for **PHY0 and PHY1** (`0x0006`, X540
   algorithm, 1000 × 5 ms), whichever port you are.
2. Wait not-busy: poll CTRL up to 100 times, 10 µs apart, until bit 31 = 0.
   The first read happens before any delay. A timeout is an error.
3. Write CTRL = `a` | (0 << 28).
4. Wait not-busy again and keep the last CTRL value.
5. If CTRL bits 19:18 ≠ 0: error (the code is in bits 27:20).
6. Otherwise read DATA.
7. Release the semaphore (with the 10 µs release delay for PHY bits).

**Write** value `v` to register `a`:

1. Acquire PHY0|PHY1. Wait not-busy.
2. Write CTRL = `a` | (0 << 28), **then** write DATA = `v`.
3. Wait not-busy. Check RESP_STAT as for a read.
4. Release.

The shared code never sets BUSY itself. It writes CTRL for a read, and CTRL
followed by DATA for a write, and hardware raises BUSY. This implies a read
starts on the CTRL write and a write starts on the DATA write. Keep exactly
this order (section 10).

**Source:** `ixgbe_type.h` (`IXGBE_SB_IOSF_*`); `ixgbe_x550.c`
`ixgbe_iosf_wait`, `ixgbe_read_iosf_sb_reg_x550`, `ixgbe_write_iosf_sb_reg_x550`.

### 7.3 KR PHY register map (IOSF target 0)

Each register has one address per port: **port 0 = `0x4xxx`, port 1 =
`0x8xxx`** (selected by lan_id). "Used on X552" means the X552 paths of the
shared code read or write the register. The rest are defined in the header and
are listed for completeness.

| Name | Port 0 | Port 1 | Used on X552 | Purpose |
|---|---|---|---|---|
| PORT_CAR_GEN_CTRL | `0x4010` | `0x8010` | no (loopback only) | bit 9 NELB_32B, bit 11 NELB_KRPCS |
| LINK_S1 | `0x4200` | `0x8200` | no | bit 28 MAC AN complete |
| **LINK_CTRL_1** | `0x420C` | `0x820C` | **yes** | Main link control, below |
| AN_CNTL_1 | `0x422C` | `0x822C` | flow control only | bit 28 sym pause, bit 29 asym pause |
| AN_CNTL_4 | `0x4238` | `0x8238` | no | bit 29 AN37-over-73 |
| AN_CNTL_8 | `0x4248` | `0x8248` | no | bit 0 linear, bit 1 limiting |
| PCS_KX_AN | `0x5918` | `0x9918` | flow control only | bits 1, 2 pause |
| PCS_KX_AN_LP | `0x591C` | `0x991C` | flow control only | bits 2, 3 LP pause |
| SGMII_CTRL | `0x42A0` | `0x82A0` | no (X553) | bit 12 force 100, bit 19 force 10 |
| LP_BASE_PAGE_HIGH | `0x436C` | `0x836C` | flow control only | bits 10, 11 LP pause |
| **DSP_TXFFE_STATE_4** | `0x4634` | `0x8634` | **yes (iXFI)** | TX FFE adaptation enables |
| **DSP_TXFFE_STATE_5** | `0x4638` | `0x8638` | **yes (iXFI)** | TX FFE adaptation enables |
| **RX_TRN_LINKUP_CTRL** | `0x4B00` | `0x8B00` | **yes (iXFI)** | bit 4 CONV_WO_PROTOCOL, bit 2 PROTOCOL_BYPASS |
| PMD_DFX_BURNIN | `0x4E00` | `0x8E00` | no (loopback) | bits 17:16 TX/RX KR loopback |
| PMD_FLX_MASK_ST20 | `0x5054` | `0x9054` | **no: X553 only** | lane mode / speed (see note) |
| **TX_COEFF_CTRL_1** | `0x5520` | `0x9520` | **yes (iXFI)** | TX coefficient override |
| RX_ANA_CTL | `0x5A00` | `0x9A00` | no | — |

**LINK_CTRL_1** (`0x420C` / `0x820C`) fields:

| Bits | Mask | Field | Use |
|---|---|---|---|
| 10:8 | `0x0000_0700` | FORCE_SPEED | `010` (`0x200`) = 1G, `100` (`0x400`) = 10G; used when AN is disabled |
| 12 | `0x0000_1000` | AN_SGMII_EN | X553 SGMII only |
| 13 | `0x0000_2000` | AN_CLAUSE_37_EN | X553 SGMII only |
| 14 | `0x0000_4000` | AN_FEC_REQ | not touched |
| 15 | `0x0000_8000` | AN_CAP_FEC | not touched |
| 16 | `0x0001_0000` | AN_CAP_KX | advertise 1000BASE-KX |
| 18 | `0x0004_0000` | AN_CAP_KR | advertise 10GBASE-KR |
| 24 | `0x0100_0000` | EEE_CAP_KX | not touched on the X552 |
| 26 | `0x0400_0000` | EEE_CAP_KR | not touched on the X552 |
| 29 | `0x2000_0000` | AN_ENABLE | clause-73 AN on/off |
| 31 | `0x8000_0000` | AN_RESTART | set to restart AN; the shared code also uses it to "toggle the port's software reset" after forcing a speed |

**DSP_TXFFE_STATE_4/5** fields: bit 6 `C0_EN`, bit 15 `CP1_CN1_EN`, bit 16
`CO_ADAPT_EN`.

**TX_COEFF_CTRL_1** fields: bit 1 `CMINUS1_OVRRD_EN`, bit 2 `CPLUS1_OVRRD_EN`,
bit 3 `CZERO_EN`, bit 31 `OVRRD_EN`.

Note on PMD_FLX_MASK_ST20: its fields are SFI mode bits 21:20 (01 SR, 10 LR,
00 DA), bit 25 SGMII_EN, bit 26 AN37_EN, bit 27 AN_EN, speed bits 30:28 (0
10M, 1 100M, 2 1G, 3 10G, 4 AN, 7 2.5G) and bit 31 "FW AN restart". The X553
uses it. The shared code guards every use with an X553 check, so an X552
driver must not write it.

**Source:** `ixgbe_type.h` (`IXGBE_KRM_*`).

### 7.4 KR AN setup (0x15AB, and the internal side of 0x15AC and of 0x15AD in KR mode)

Given the speeds to advertise, `S` (default capability: 10G + 1G):

1. If `S` includes 2.5G, leave the link alone. That cannot happen on the X552
   and is listed only for completeness.
2. If `MMNGC.MNG_VETO` is set, do nothing. This check is skipped when the
   routine is called from the SFP path (7.7.6) or the 10G_T KR path (7.8.5).
3. IOSF read LINK_CTRL_1(lan_id). Set `AN_ENABLE` (bit 29). Clear `AN_CAP_KR`
   (bit 18) and `AN_CAP_KX` (bit 16). Set `AN_CAP_KR` if 10G ∈ `S`, and
   `AN_CAP_KX` if 1G ∈ `S`. IOSF write it back.
4. **Restart AN:** IOSF read LINK_CTRL_1 again, set `AN_RESTART` (bit 31),
   IOSF write it. There is no wait and no poll.
5. Link status comes from LINKS (section 8). The shared code does not read
   LINK_S1.

The shared code never clears `AN_RESTART` explicitly. Whether it self-clears
is not stated (section 10).

**Source:** `ixgbe_x550.c` `ixgbe_setup_kr_x550em`, `ixgbe_setup_kr_speed_x550em`,
`ixgbe_restart_an_internal_phy_x550em`; call path `ixgbe_setup_mac_link_X540` →
`ixgbe_setup_phy_link_speed_generic` → `ixgbe_setup_phy_link`.

### 7.5 iXFI forced mode (the internal side of 0x15AD)

Force speed `F` (10G or 1G; anything else is an error). Only for the X552 MAC.

1. IOSF RMW LINK_CTRL_1(lan_id): clear `AN_ENABLE` (bit 29) and FORCE_SPEED
   (bits 10:8). Set FORCE_SPEED = `100` (10G) or `010` (1G).
2. X552-specific extra configuration, in this order, each an IOSF RMW:
   1. RX_TRN_LINKUP_CTRL: set bit 4 (`CONV_WO_PROTOCOL`), which disables the
      training-protocol FSM.
   2. DSP_TXFFE_STATE_4: clear bits 6, 15, 16.
   3. DSP_TXFFE_STATE_5: clear bits 6, 15, 16. Steps 2 and 3 stop the flex
      logic from training the TX FFE.
   4. TX_COEFF_CTRL_1: set bits 31, 3, 2, 1, which enable the coefficient
      overrides.
   Any IOSF error aborts the sequence.
3. Restart AN on LINK_CTRL_1 (7.4 step 4). Here it toggles the port's
   software reset so the forced speed takes effect.

**Source:** `ixgbe_x550.c` `ixgbe_setup_ixfi_x550em`, `ixgbe_setup_ixfi_x550em_x`.

### 7.6 NW_MNG_IF_SEL (`0x11178`)

Read-only board configuration. The shared code reads it once at PHY init.

| Bits | Mask | Field | Use on the X552 |
|---|---|---|---|
| 1 | `0x0000_0002` | MDIO_ACT | (X553: MDIO connected to an external PHY) |
| 2 | `0x0000_0004` | MDIO_IF_MODE | not used |
| 7:3 | `0x0000_00F8` | MDIO_PHY_ADD | the address of the external PHY; the only address probed on 10G_T when the register is non-zero (2.5) |
| 13 | `0x0000_2000` | EN_SHARED_MDIO | not used |
| 17–21 | — | PHY_SPEED_10M/100M/1G/2.5G/10G | only 2.5G is checked, and only on the X553 |
| 24 | `0x0100_0000` | INT_PHY_MODE | **X552 only:** 1 = internal link to the X557 is KR (AN); 0 = iXFI (forced) |
| 25 | `0x0200_0000` | SGMII_ENABLE | not used |

**Source:** `ixgbe_type.h`; `ixgbe_x550.c` `ixgbe_read_mng_if_sel_x550em`,
`ixgbe_setup_internal_phy_t_x550em`, `ixgbe_setup_mac_link_t_X550em`.

### 7.7 X552 SFP (0x15AC): CS4227 retimer and SFP+

#### 7.7.1 I2C mux and semaphore

- PHY semaphore mask = `0x1806` (PHY0 | PHY1 | I2C0 | I2C1), for every SFP and
  CS4227 I2C access, and for the MAC reset.
- **ESDP setup**, at PHY init and again after every MAC reset:
  - Port 1 only: clear `SDP1_NATIVE` (bit 17) and `SDP1` (bit 1), and set
    `SDP1_DIR` (bit 9). SDP1 becomes an output, driven low.
  - Both ports: clear `SDP0_NATIVE` (bit 16) and `SDP0_DIR` (bit 8). SDP0
    becomes an input; it is the cage-full signal of 4.4.
  - Flush.
- **Mux:** on port 1 only, taking any semaphore that includes an I2C bit also
  sets ESDP `SDP1` (bit 1) and flushes. Releasing clears `SDP1` first, before
  the semaphore bits are cleared. Port 0 never touches SDP1. The mux therefore
  routes the shared segment to port 1's I2C master while port 1 holds the
  semaphore.

#### 7.7.2 CS4227 reset: once per power-on, shared by both ports

The two ports share one CS4227. A scratch register on the CS4227 records
whether a port has already reset it. All CS4227 accesses use the I2C combined
format (3.6) at device `0xBE`. The port-expander accesses are plain byte
transfers (3.3) at `0xE0`. Everything inside steps 3 and 4 uses the *unlocked*
I2C primitives while the semaphore is held.

| CS4227 register | Address | Values |
|---|---|---|
| GLOBAL_ID_LSB / MSB | `0x0000` / `0x0001` | ID value `0x03E5` (defined, not checked) |
| SCRATCH | `0x0002` | `0x1357` = reset pending; `0x5AA5` = reset complete |
| EFUSE_STATUS | `0x0181` | `0x0001` = load OK |
| EFUSE_PDF_SKU | `0x019F` | `0x0014` CS4227 (dual), `0x0010` CS4223 (quad; X553 only) |
| LINE_SPARE22_MSB | `0x12AD` | speed (`0x8000` = 1G, 0 = 10G). Defined, **not written** |
| LINE_SPARE24_LSB | `0x12B0` | EDC mode, line side (7.7.5) |
| HOST_SPARE22_MSB | `0x1AAD` | speed, host side. Defined, not written |
| HOST_SPARE24_LSB | `0x1AB0` | EDC mode, host side. Defined, not written |
| EEPROM_STATUS | `0x5001` | bit 0 = EEPROM load OK |

Port expander at `0xE0`: register 1 = output, register 3 = configuration
(direction). Bit 1 drives the CS4227 reset line.

**Check-and-reset procedure** (run at PHY init on each port):

1. Up to 15 tries:
   1. Acquire the `0x1806` semaphore. If that fails: wait 30 ms and try again.
   2. Read SCRATCH. If the read succeeds and the value is `0x5AA5`, the CS4227
      is already reset: release, wait 10 ms, done.
   3. If the read failed, or the value is not `0x1357`: go to step 3, holding
      the semaphore.
   4. The value is `0x1357` (the other port is resetting): release, wait 30
      ms, try again.
2. If all 15 tries saw "pending", assume the other port died. Acquire the
   semaphore; if that fails, give up.
3. **Hard reset through the port expander**, all under the semaphore:
   1. PE register 1: set bit 1.
   2. PE register 3: clear bit 1 (the pin becomes an output).
   3. PE register 1: clear bit 1 (reset asserted).
   4. Wait **500 µs**.
   5. PE register 1: set bit 1 (reset released).
   6. Wait **450 ms**.
   7. Read EFUSE_STATUS up to 15 times, 30 ms apart, until the read succeeds
      and returns `0x0001`. Otherwise the reset failed.
   8. Read EEPROM_STATUS. It must succeed with bit 0 set.
   Any failure: release, wait 10 ms, stop (report an error).
4. **Handshake:**
   1. Write SCRATCH = `0x1357`.
   2. Release the semaphore, wait 10 ms, re-acquire it (giving up on failure).
   3. Write SCRATCH = `0x5AA5`.
   4. Release, wait 10 ms.

The semaphore is held for the whole hard reset, so a peer that arrives during
the reset blocks in its acquire; the X552 resource timeout is 5 s. "Pending"
is written only *after* the reset, and is visible only during the 10 ms
release window before "complete" is written. The scratch value survives
software-only restarts and is lost only when the CS4227 is power-cycled or
reset. That is how "once per power-on" works.

#### 7.7.3 Module identification

Module identification is SFF-8472 (section 4) over the same shared segment.
The X552 I2CCTL layout is used (3.1) with the `0x1806` semaphore and the mux.
Identification runs after the CS4227 check.

#### 7.7.4 Supported module types (X552 rule on top of 4.2)

| sfp_type | Supported | "Linear" (drives the EDC mode) |
|---|---|---|
| `da_cu_core0/1` (passive DA) | yes | **yes** |
| `srlr`, `da_act_lmt`, `1g_sx`, `1g_lx`, `1g_bx`, `10g_bx` | yes | no |
| `1g_cu` (1000BASE-T SFP) | **no** | — |
| `unknown` | no | — |
| `not_present` | "not present" (link setup returns success, does nothing) | — |

#### 7.7.5 Capabilities and rate select

- AN is never used ("CS4227 SFP must not enable auto-negotiation").
- 1G module types (sx, lx, bx): 1G only. Otherwise 10G, plus 1G if multispeed.
- The laser-control functions are **absent** on the X552. Rate select is soft
  (4.3).
- `setup_link` is the multispeed algorithm of 5.8, with the per-speed step
  below in place of 82599 `setup_mac_link`, and with no laser flap.

#### 7.7.6 Per-speed setup (`setup_mac_link` for 0x15AC)

For one speed `F`:

1. Check the module (7.7.4). Not present: return success with nothing done.
   Unsupported: error.
2. **KR PHY:** 7.4 steps 3–4 with `S` = `F`: AN enabled, `AN_CAP_KR` if 10G,
   `AN_CAP_KX` if 1G, then restart. This path does not check the veto.
3. **CS4227 EDC mode, line side:** register = `0x12B0` + (lan_id << 12), so
   `0x12B0` for port 0 and `0x22B0` for port 1. Value = (EDC << 1) | 1, with
   EDC = `0x0002` (CX1, copper) for linear modules and `0x0004` (SR, optical)
   otherwise. The values written are therefore **`0x0005`** (DA) or
   **`0x0009`** (optical). This is one locked combined write (takes and
   releases `0x1806`, mux included).
4. The multispeed wrapper then polls LINKS (5.8).

The CS4227 speed registers (`0x12AD`/`0x1AAD`) and the host-side EDC register
are never written by the shared code. The CS4227 host side is left at its
EEPROM defaults.

**Source:** `ixgbe_phy.h` (`IXGBE_CS4227_*`, `IXGBE_PE*`); `ixgbe_x550.c`
`ixgbe_setup_mux_ctl`, `ixgbe_set_mux`, `ixgbe_read_cs4227`,
`ixgbe_write_cs4227`, `ixgbe_read_pe`, `ixgbe_write_pe`, `ixgbe_reset_cs4227`,
`ixgbe_check_cs4227`, `ixgbe_supported_sfp_modules_X550em`,
`ixgbe_identify_sfp_module_X550em`, `ixgbe_setup_sfp_modules_X550em`,
`ixgbe_setup_mac_link_sfp_x550em`, `ixgbe_get_link_capabilities_X550em`,
`ixgbe_init_mac_link_ops_X550em`, `ixgbe_reset_hw_X550em`; `ixgbe_common.c`
`ixgbe_setup_mac_link_multispeed_fiber`, `ixgbe_set_soft_rate_select_speed`.

### 7.8 X552 10GBASE-T (0x15AD): X557 external PHY

#### 7.8.1 Order of operations in the shared code's reset

1. Clear `HLREG0.MDCSPD` (2.4).
2. Identify the PHY (2.5): ID `0x0154_0240` or `0x0154_0250`. The semaphore
   mask is PHY0 or PHY1 by lan_id.
3. **Unstall the PHY firmware** (7.8.2).
4. PHY reset (2.6, X557 variant), then enable the alarms (7.8.3).
5. MAC reset: `LNK_RST` if the link is down, `RST` if it is up, under the PHY
   semaphore. Poll 10 × 1 µs, wait 50 ms.
6. Clear `HLREG0.MDCSPD` again.

#### 7.8.2 Power-up stall release

1. Read `1.0xCC02`. If bits 1:0 are non-zero, the PHY firmware has just come
   out of reset and this is the first software instance since power-on.
2. In that case RMW `30.0xC479`: clear bit 15 (`POWER_UP_STALL`). Without this
   the X557 firmware stays stalled and never brings the copper link up.
3. If bits 1:0 are zero, do nothing.

#### 7.8.3 Alarm (LASI) enables after reset

These are optional for a polling driver. The shared code performs them as
part of the reset.

1. Read the alarm chain once to clear latched flags (7.8.7).
2. RMW `7.0xD401` bit 0 = 1 (link-status alarm). X552 only.
3. RMW `30.0xD400` bits 14 and 4 = 1 (high-temperature and device-fault
   alarms).
4. RMW `30.0xFF01` bits 12 and 2 = 1 (AN vendor alarm, global alarm 1).
5. RMW `30.0xFF00` bit 0 = 1 (chip-wide vendor alarm).

#### 7.8.4 Advertisement

Capabilities come from `1.0x0004` (6.4). The X552 then **removes 100M**, so
the capabilities are 10G and 1G. The advertisement is exactly the X540
sequence of 6.5: `7.0x0020` bit 12, `7.0xC400` bit 15, `7.0x0010` bits 8/7
(both end up clear), then restart AN at `7.0x0000` bit 9 unless vetoed.

#### 7.8.5 Link setup (`setup_link` for 0x15AD)

Given requested speeds `S` (default 10G + 1G):

1. `F` = 10G if 10G ∈ `S`, else 1G.
2. If `NW_MNG_IF_SEL` bit 24 is **clear** (iXFI internal link):
   1. Force iXFI to `F` (7.5).
   2. Poll the link status (7.8.6) every 100 ms, up to 10 times (1 s). Stop
      early if it is up; not being up is not an error.
3. Advertise `S` on the X557 and restart AN (7.8.4).

If bit 24 is **set** (KR internal link), step 2 is skipped. The internal link
is set to KR AN with 10G + 1G when the copper link changes (next section),
with no veto check.

#### 7.8.6 Link status and speed tracking

**Link up** requires both:

1. LINKS bit 30 set (MAC side, section 8), and
2. X557 `7.0x0001` bit 2 set, read **twice back-to-back** and taking the
   second value. The bit latches low: the first read clears a stale "down".

**Speed:** LINKS reports the internal link speed. The copper speed is in
`7.0xC800` bits 2:0 (table in 6.6).

**Re-forcing iXFI when the copper speed changes.** An interrupt-driven driver
does this on the X557 link-status alarm. A polling driver must do it when it
sees the copper link come up. The steps:

1. Only for copper media. If bit 24 is set (KR internal): write KR AN with
   10G + 1G (7.4 steps 3–4, no veto check) and stop.
2. Read the copper link (`7.0x0001` twice). If it is down, nothing to do.
3. Read `7.0xC800`, then check the copper link again. If it is now down,
   nothing to do.
4. `7.0xC800` bits 2:0 = `111` (10G full) → force iXFI 10G. `101` (1G full)
   → force iXFI 1G. Any other value is "invalid link settings": the internal
   PHY supports only 10G and 1G, so do not report link up.

The practical consequence: copper that negotiates **1G** while iXFI is still
forced to 10G gives a MAC link that stays down (or a mismatch) until this
re-force runs.

#### 7.8.7 Alarm chain (for reference; clears latched status)

1. `30.0xFC00` bit 0? If clear, stop.
2. `30.0xFC01` bits 12 or 2? If neither, stop.
3. `30.0xCC00`: bit 14 means high-temperature failure: power the PHY down
   (`30.0x0000` bit 11) and report over-temperature. Bit 4 is a device fault:
   read `30.0xC850`, and if it is `0x8007` treat it as over-temperature too.
4. `7.0xFC00` bit 9? If clear, stop.
5. `7.0xCC01` bit 0 = link-state change. That is the trigger for 7.8.6.

#### 7.8.8 Low-power link-up (LPLU)

The shared code installs an LPLU handler only when `FUSES0_GROUP(0)`
(`0x11158`) bits 7:6 are 0 (the first X552 silicon revision). It is used when
entering D3 or Wake-on-LAN, and a boot driver does not need it.

**Source:** `ixgbe_x550.c` `ixgbe_set_mdio_speed`, `ixgbe_reset_hw_X550em`,
`ixgbe_init_ext_t_x550em`, `ixgbe_reset_phy_t_X550em`,
`ixgbe_enable_lasi_ext_t_x550em`, `ixgbe_get_lasi_ext_t_x550em`,
`ixgbe_handle_lasi_ext_t_x550em`, `ixgbe_setup_mac_link_t_X550em`,
`ixgbe_setup_internal_phy_t_x550em`, `ixgbe_ext_phy_t_x550em_get_link`,
`ixgbe_check_link_t_X550em`, `ixgbe_init_phy_ops_X550em`; `ixgbe_phy.c`
`ixgbe_get_copper_speeds_supported`, `ixgbe_setup_phy_link_generic`,
`ixgbe_reset_phy_generic`.

### 7.9 X552 1G copper (0x15AE): Marvell external PHY

What the BSD shared code does, in full (confirmed against the DPDK copy of the
same base code):

- The PHY type is set from the device ID (`ext_1g_t`) with **no MDIO probe**.
  The Marvell IDs `0x0141_0DD0` (88E1500) and `0x0141_0EA0` (88E1543) are in
  the ID table, but nothing probes for them on this device.
- The raw MDIO operations are removed for this device ID. The shared code
  never issues an MDIO cycle to this PHY.
- "Link is managed by firmware": there is no PHY setup, no PHY reset, no flow
  control setup and no LED control. The generic link-setup call reaches a
  missing PHY setup routine and does nothing.
- Capabilities: 1G only. Link status and speed come from LINKS (section 8).
- The `phy_semaphore_mask` is left 0.

The shared code has **no Marvell register programming** for the X552. A boot
driver for 0x15AE should do nothing to the PHY and poll LINKS. If the link does
not come up, that is a board firmware matter, not a driver step that is
missing. The Marvell PHY's own register set (clause 22, pages) is outside the
sources allowed here (section 10).

**Source:** `ixgbe_x550.c` `ixgbe_init_ops_X550EM`, `ixgbe_init_ops_X550EM_x`,
`ixgbe_identify_phy_x550em`, `ixgbe_init_phy_ops_X550em`,
`ixgbe_init_mac_link_ops_X550em`, `ixgbe_get_link_capabilities_X550em`;
`ixgbe_phy.c` `ixgbe_get_phy_type_from_id`.

### 7.10 X552 KX4 (0x15AA) and XFI (0x15B0)

The PHY setup routine is absent and the raw PHY register operations return
"not implemented". The link is run by hardware from the NVM configuration.
Capabilities: KX4 is 10G + 1G with AN; XFI is 10G + 1G with no AN. Link comes
from LINKS. A boot driver only polls.

**Source:** `ixgbe_x550.c` `ixgbe_init_phy_ops_X550em`,
`ixgbe_get_link_capabilities_X550em`, `ixgbe_get_supported_physical_layer_X550em`.

### 7.11 X552 MAC reset (link-relevant parts)

1. Stop the adapter, clear `HLREG0.MDCSPD` (10G_T), run PHY init (7.1, which
   includes the CS4227 check on 0x15AC). An unsupported SFP or an invalid PHY
   address aborts the reset.
2. 10G_T: unstall the X557 (7.8.2).
3. SFP: module setup (support check only; there is no NVM sequence).
4. PHY reset where one exists (X557 only), unless vetoed. Over-temperature
   aborts the reset.
5. `CTRL.LNK_RST` if the link is down, `CTRL.RST` if it is up. Written under
   `phy_semaphore_mask`: 10G_T is PHY0/PHY1, SFP is `0x1806` with the mux,
   and the others take no bits. Poll 10 × 1 µs, wait 50 ms.
6. Clear `HLREG0.MDCSPD` again (10G_T). Redo the SFP ESDP setup (7.7.1).

**Source:** `ixgbe_x550.c` `ixgbe_reset_hw_X550em`.

---

## 8. Link status, speed and duplex

### 8.1 LINKS (`0x042A4`)

| Bits | Mask | Field (per the shared code) |
|---|---|---|
| 31 | `0x8000_0000` | KX_AN_COMP: backplane AN complete (82599 AUTOC AN modes) |
| 30 | `0x4000_0000` | LINK_UP |
| 29:28 | `0x3000_0000` | Speed: `11` 10G, `10` 1G, `01` 100M, `00` 10M (X553 only) |
| 27 | `0x0800_0000` | NON_STD speed: with `11`, means 2.5G (X550 and later MACs, including the X552) |
| 22 | `0x0040_0000` | XGXS enabled |
| 21 | `0x0020_0000` | 1G PCS enabled |
| 20 | `0x0010_0000` | 1G AN enabled |
| 19 | `0x0008_0000` | KX AN idle |
| 18 | `0x0004_0000` | 1G sync |
| 17 | `0x0002_0000` | 10G lanes aligned |
| 16, 14:12 | `0x0001_7000` | 10G lane sync |
| 12 | `0x0000_1000` | TL fault |
| 11:8 | `0x0000_0F00` | Signal detect per lane |

Decoding in the shared code:

1. If the crosstalk fix is active (4.4) and the cage is empty: link down,
   speed unknown.
2. Read LINKS twice. The first read is only compared with the second, to log
   a change.
3. Link up = bit 30. With the crosstalk fix active, a "up" is confirmed by
   reading LINKS again 5 ms later.
4. Speed from bits 29:28, as in the table. All speeds are **full duplex**:
   the shared code has no half-duplex state and no duplex field.
5. On request, the shared code polls bit 30 every 100 ms up to 90 times
   (**9 s**, `LINK_UP_TIME`).

Device-specific additions:

- **X552 10G_T:** also require X557 `7.0x0001` bit 2 (7.8.6).
- **82599 T3 LOM:** the PHY's own status is `30.0x0001` (5.11). The MAC uses
  LINKS.
- **X540:** LINKS only.

**Source:** `ixgbe_common.c` `ixgbe_check_mac_link_generic`;
`ixgbe_x550.c` `ixgbe_check_link_t_X550em`; `ixgbe_type.h` (`IXGBE_LINKS_*`).

### 8.2 Timeouts and delays, in one place

| What | Value | Where |
|---|---|---|
| SWSM.SMBI / SWESMBI / REGSMP poll | 2000 × 50 µs (100 ms) each | 1.4 |
| Resource semaphore | 200 × 5 ms (82599, X540); 1000 × 5 ms (X552) | 1.4 |
| Release delay | 10 µs (PHY/MNG bits), 2 ms (others, X540+) | 1.4.3 |
| MDIO cycle | 100 × 10 µs (1 ms) per cycle | 2.1 |
| IOSF busy | 100 × 10 µs (1 ms) per wait | 7.2 |
| PHY soft reset | 30 × 100 ms (3 s) | 2.6 |
| I2C clock stretch | 500 × 1 µs | 3.2 |
| I2C ACK | 10 × 1 µs after 4 µs | 3.2 |
| I2C failed read, locked | 100 ms before retry | 3.3 |
| QSFP bus grant (82599) | 200 × 5 ms (1 s) | 3.5 |
| Pipeline reset AN-state poll | 10 × 4 ms | 5.4 |
| NVM semaphore delay | 10 ms | 5.5, 7.7.2 |
| AN completion (82599 AN modes) | 45 × 100 ms (4.5 s) | 5.7 |
| Post-AUTOC settle | 50 ms | 5.7 |
| Multispeed 10G attempt | 40 ms + up to 10 × 100 ms | 5.8 |
| Multispeed 1G attempt | 40 ms + 100 ms | 5.8 |
| SmartSpeed | 3 × (5 × 100 ms), then 6 × 100 ms | 5.10 |
| Laser off / on | 100 µs / 100 ms | 5.9 |
| MAC reset | 10 × 1 µs poll, then 50 ms (82599, X552) / 100 ms (X540) | 5.13, 6.7, 7.11 |
| CS4227 reset | 500 µs hold, 450 ms, then 15 × 30 ms | 7.7.2 |
| CS4227 peer wait | 15 × 30 ms | 7.7.2 |
| X552 10G_T iXFI link wait | 10 × 100 ms | 7.8.5 |
| Link-up wait (generic) | 90 × 100 ms (9 s) | 8.1 |

---

## 9. Minimal link-up walkthroughs for a polling UEFI driver

These follow the shared code's order with the policy choices for a boot
driver stated. They assume BAR0 is mapped, the MAC reset and NVM access
described elsewhere (bring-up) are available, and `lan_id` has been read.

The common first steps for every device:

1. Read `MMNGC`. Remember `MNG_VETO`.
2. X540/X552: run the semaphore clean-up (1.4.3) once, or decide to treat a
   timeout as "hands off" (see the recommendation there).
3. Decide the requested speeds = the device's capabilities (sections 5.6, 6.4,
   7.7.5, 7.8.4). A boot driver does not need a speed override.

### 9.1 82599 SFP+ (0x10FB, 0x1507, 0x1529, 0x154A, 0x154D, 0x1557)

1. Optional: check ESDP `SDP2` for cage-full (4.4).
2. Identify the module (4.2) through I2CCTL `0x28` under PHY0/PHY1.
   - Not present: report no link, and optionally re-probe on each poll.
   - Unsupported or unknown: report it and stop.
3. MAC reset (5.13), keeping or taking the AUTOC/AUTOC2 snapshot.
4. Module setup (5.5): NVM list → CORECTL under MAC_CSR → 10 ms → protected
   AUTOC write with LMS 10G serial. Redo this after any later MAC reset.
5. If manageability is not enabled: enable the laser (clear `SDP3`, 100 ms).
   Check `SDP3_DIR` first (5.9).
6. Multispeed module: run 5.8 (it rate-selects, programs, and polls up to about
   1.2 s). Single-speed module: `setup_mac_link` (5.7) for the one speed.
7. Poll LINKS bit 30 every 100 ms. A budget of 3–9 s is reasonable. Report
   speed from bits 29:28.

### 9.2 82599 backplane and CX4 (0x10F7, 0x1514, 0x1517, 0x10F8, 0x152A, 0x10FC, 0x10F9)

1. MAC reset and snapshot (5.13).
2. Capabilities from the snapshot's LMS (5.6).
3. If LESM is enabled, or SmartSpeed is not wanted: `setup_mac_link` (5.7)
   with waiting on, which polls `KX_AN_COMP` up to 4.5 s in AN modes.
   Otherwise use SmartSpeed (5.10).
4. Poll LINKS.

### 9.3 82599 T3 LOM (0x151C)

1. MDIO scan (2.5) under PHY0/PHY1. Expect TN1010.
2. PHY reset (2.6) unless vetoed or over-temperature.
3. MAC reset and snapshot.
4. Advertise and restart (5.11), then restart the MAC side (pipeline reset,
   50 ms).
5. Poll LINKS. `30.0x0001` gives the PHY's view.

### 9.4 82599 QSFP (0x1558) and bypass (0x155D)

- QSFP: as 9.1, but set up the bus handshake (3.5) first and identify with
  4.5. There is no rate select.
- Bypass: media "fiber fixed", always multispeed, soft rate select (4.3). The
  rest is as 9.1. Bypass-relay control is out of scope.

### 9.5 X540 (0x1528, 0x1560, 0x155C)

1. MAC reset (6.7): PHY semaphore, `CTRL.RST`, 100 ms.
2. MDIO scan: expect `0x0154_0200` (6.2).
3. Power the PHY on: clear `30.0x0000` bit 11 (6.3).
4. Read the capabilities from `1.0x0004`. Advertise all of them and restart
   AN (6.5), skipping the restart if vetoed.
5. Poll LINKS bit 30 every 100 ms. 10GBASE-T AN plus training commonly takes
   several seconds; the shared code's own wait limit is 9 s. Speed from bits
   29:28.

### 9.6 X552 KR (0x15AB)

1. MAC reset (7.11). No semaphore bits.
2. Unless vetoed: KR AN setup with 10G + 1G (7.4) over IOSF. This takes the
   PHY0|PHY1 semaphore for each IOSF access.
3. Poll LINKS.

### 9.7 X552 KX4 (0x15AA), XFI (0x15B0), 1G_T (0x15AE)

1. MAC reset (7.11).
2. No PHY programming at all (7.9, 7.10).
3. Poll LINKS. 1G_T reports 1G; the others report 10G or 1G.

### 9.8 X552 SFP (0x15AC)

1. ESDP mux setup (7.7.1). Semaphore mask `0x1806`.
2. CS4227 check-and-reset (7.7.2). In the worst case this takes about 0.5 s
   plus the peer waits.
3. Identify the module (7.7.3) with I2CCTL `0x15F5C`. Not present: report no
   link. Unsupported: stop.
4. MAC reset (7.11), then redo the ESDP mux setup.
5. Multispeed wrapper (5.8) with the per-speed step 7.7.6 (KR AN cap +
   restart, then write `0x0005` or `0x0009` to CS4227 `0x12B0 + (lan_id <<
   12)`) and soft rate select. No laser control.
6. Poll LINKS.

### 9.9 X552 10GBASE-T (0x15AD)

1. Clear `HLREG0.MDCSPD`.
2. Read `NW_MNG_IF_SEL`. Probe the X557 at bits 7:3 if the register is
   non-zero, else scan (2.5).
3. Unstall (7.8.2).
4. PHY reset (2.6, X557 variant) unless vetoed, then optionally the alarm
   enables (7.8.3).
5. MAC reset (7.11), then clear `MDCSPD` again.
6. Link setup (7.8.5): if bit 24 is clear, force iXFI 10G and wait up to 1 s;
   then advertise 10G + 1G and restart AN.
7. Poll every 100 ms:
   1. When `7.0x0001` bit 2 (read twice) is up, run the re-force of 7.8.6
      (read `7.0xC800`, force iXFI to 10G or 1G, or KR AN in KR mode). Do this
      once per copper link-up or speed change.
   2. Report link up only when LINKS bit 30 **and** the X557 link bit are
      both set.

---

## 10. Uncertainties to verify on hardware or against the datasheets

1. **Datasheets not consulted.** All register semantics here come from the
   BSD shared code, because intel.com returned HTTP 403. The following should
   be checked against the datasheets: MSCA opcode meanings, LINKS bit fields,
   the AUTOC bit 18 overlap between FECA and AN_RX_ALIGN, and the meaning of
   the ESDP pins on each board.
2. **82599_LS (0x154F)** appears in FreeBSD's PCI match table and in the
   82599 media switch ("fiber LCO"), but **not** in the shared code's MAC-type
   table (`ixgbe_set_mac_type`). The shared code would therefore not bring it
   up as an 82599. Section 11.1 derives its path from the media type; it is
   still unverified on hardware.
3. **X540 PHY MDIO address.** The shared code scans 0–31 and takes the first
   responder. Whether each port's MDIO bus reaches only its own PHY, and at
   which address, should be confirmed by logging the scan on hardware.
4. **X552 10G_T PHY address.** When `NW_MNG_IF_SEL` is non-zero, only bits 7:3
   are probed, and a miss is fatal. The header comments describe this field
   as used on the X553. Log `NW_MNG_IF_SEL` and the X557's real address on an
   X552 board.
5. **X552 `INT_PHY_MODE` (bit 24).** The shared code treats set = KR and
   clear = iXFI. Which value real X552 10G_T boards strap should be recorded.
6. **IOSF trigger semantics.** A read starts on the CTRL write, a write on the
   DATA write (7.2). This is inferred from the order of the shared code's
   accesses; keep that order exactly.
7. **KR `AN_RESTART` (LINK_CTRL_1 bit 31) self-clearing.** It is set with RMW
   and never cleared by the shared code.
8. **CS4227 read checksum** is received but not verified by the shared code.
   A driver may check it, but the expected formula for the reply is not in
   the source.
9. **CS4227 scratch handshake** relies on the CS4227 keeping `0x5AA5` across
   host resets. After a platform reset that does not power-cycle the CS4227,
   the reset is skipped by design. Verify the first boot after AC power-on
   and a warm reboot both come up.
10. **82599 module setup ORs LMS `011`** into the original AUTOC instead of
    replacing the field (5.5 step 6). Confirm that the NVM default LMS on each
    SFP SKU is `000` or `011`.
11. **Laser control (SDP3)** assumes the NVM made SDP3 an output. Check ESDP
    bit 11 on hardware.
12. **Module presence polarity** (82599 SDP2, X552 SDP0: set = cage full) is
    taken from the shared code. Verify with an empty cage.
13. **X540 speed decode from `7.0xC800`** (6.6) is inferred from the X557 path.
    The X540 shared code uses LINKS only.
14. **X552 1G_T Marvell PHY:** the BSD shared code (FreeBSD and DPDK) does
    nothing to it. If a board needs driver-side PHY programming, it would
    need the Marvell datasheet, which is not in the permitted sources.
15. **Semaphore force-take** after a firmware timeout (1.4.3) is shared-code
    policy. Choose deliberately whether a boot driver copies it.

---

## 11. Addendum (#16, 2026-10-06): gaps closed from the shared code

Added by the driver's maintainers, not by this document's author, to close
three points the sections above left open. Same rules: register layouts and
sequences from Intel's BSD-3-Clause shared code, FreeBSD `sys/dev/ixgbe/` at
commit `32b8381d711c` (2026-09-18); no code reproduced.

### 11.1 82599_LS (0x154F): media "fiber LCO"

The shared code has no MAC type for 0x154F (section 10 item 2), but every
82599 decision that follows the MAC type keys off the media type, and
`ixgbe_get_media_type_82599` gives 0x154F `fiber_lco`. With that media:

1. **No module identification.** The module ID runs only for `fiber` (SFP+)
   and `fiber_qsfp`; for any other media no module is reported present, and
   with no MDIO PHY found the PHY type is "none".
2. **No laser control and no rate select.** The laser operations are
   installed only for `fiber` media; rate select runs only inside multispeed.
3. **Not multispeed.** Only module identification (or the bypass part's
   media) sets multispeed.
4. **Link:** therefore `setup_mac_link` (5.7) with the capabilities from the
   original AUTOC (5.6), as for a backplane. SmartSpeed is offered only to
   `backplane` media, so it does not apply.
5. **No crosstalk fix** (4.4): it covers `fiber` and `fiber_qsfp` only.

Linux's ixgbe binds 0x154F as an ordinary 82599 board (its PCI table, read
for behaviour only), which is consistent with this path.

**Source:** `ixgbe_82599.c` `ixgbe_get_media_type_82599`,
`ixgbe_init_mac_link_ops_82599`, `ixgbe_identify_phy_82599`; `ixgbe_phy.c`
`ixgbe_identify_module_generic`; `ixgbe_common.c` `ixgbe_need_crosstalk_fix`.

### 11.2 X552 NVM access: the firmware host interface

The X552 reads NVM (shadow RAM) words through a command to its manageability
firmware, not through EERD. One word:

1. Take SW_FW_SYNC `SW_MNG` (bit 10) and `EEP` (bit 0) for the whole
   exchange (section 1.4; `EEP` is also blocked by the hardware's FLASH bit).
2. FWSTS (`0x15F0C`): set bit 9 (FWRI, firmware-reset indication; write 1).
3. HICR (`0x15F00`): bit 0 (EN) must be set, else the host interface is
   disabled and the read fails.
4. Write the 16-byte command block, as four little-endian dwords, to
   FLEX_MNG (`0x15800` + 4·i):
   - dword 0: command `0x31` (read shadow RAM), length high `0x00`, length
     low `0x06`, checksum `0xFF` (fixed) → `0xFF060031`;
   - dword 1: the **byte** address (word × 2) as a big-endian 32-bit value;
   - dword 2: the length in bytes (2) as a big-endian 16-bit value, then 16
     bits of padding → `0x00000200`;
   - dword 3: zero (data and padding).
5. Set HICR bit 1 (C, command pending), keeping the value read in step 3.
6. Poll HICR until C clears, up to 500 ms.
7. Fail if C is still set or HICR bit 2 (SV, status valid) is clear.
8. The word is the low 16 bits of FLEX_MNG dword 3.
9. Release the semaphores.

The shared code does not check the response's status byte for this command.

**Source:** `ixgbe_x550.c` `ixgbe_read_ee_hostif_X550`; `ixgbe_common.c`
`ixgbe_hic_unlocked`, `ixgbe_get_device_caps_generic`,
`ixgbe_start_hw_generic`; `ixgbe_type.h` (`IXGBE_HICR*`, `IXGBE_FWSTS_FWRI`,
`IXGBE_FLEX_MNG`, `FW_READ_SHADOW_RAM_*`, `FW_NVM_DATA_OFFSET`,
`IXGBE_HI_COMMAND_TIMEOUT`, `struct ixgbe_hic_read_shadow_ram`).

### 11.3 QSFP+ multispeed (82599 0x1558)

After identifying a QSFP+ module (4.5), the module is multispeed when byte
`0x86` (1G compliance) and byte `0x83` (10G compliance) pair up as 1000BASE-SX
(`0x86` bit 0) with 10GBASE-SR (`0x83` bit 4), or 1000BASE-LX (`0x86` bit 1)
with 10GBASE-LR (`0x83` bit 5). Unlike SFP+ (4.2), DA cables are not
multispeed. A multispeed QSFP runs the multispeed algorithm (5.8) with no
rate select (the module follows the MAC's speed) and no laser flap (no laser
control on QSFP), and without full auto-negotiation (5.6), so its 1G step is
1G SFI without AN.

**Source:** `ixgbe_phy.c` `ixgbe_identify_qsfp_module_generic`;
`ixgbe_common.c` `ixgbe_setup_mac_link_multispeed_fiber`; `ixgbe_82599.c`
`ixgbe_get_link_capabilities_82599`, `ixgbe_init_mac_link_ops_82599`.

### 11.4 Crosstalk fix: which devices

The fix (4.4) is decided once, after the MAC reset, for the 82599 and the
X552 from NVM word `0x2C` bit 7, and then applies only to `fiber` and
`fiber_qsfp` media. So it covers the 82599 SFP+ and QSFP+ devices (cage pin
SDP2) and the X552 SFP device 0x15AC (cage pin SDP0), not the bypass part
(`fiber_fixed`), 0x154F or any backplane. The check applies to every link
read, including those inside the multispeed loop.

**Source:** `ixgbe_common.c` `ixgbe_start_hw_generic`,
`ixgbe_need_crosstalk_fix`, `ixgbe_check_mac_link_generic`.

---

## Appendix A. Constants

### A.1 CSR offsets

| Name | Offset |
|---|---|
| CTRL | `0x00000` |
| STATUS | `0x00008` |
| ESDP | `0x00020` |
| I2CCTL (82599, X540) | `0x00028` |
| HLREG0 | `0x04240` |
| MSCA | `0x0425C` |
| MSRWD | `0x04260` |
| AUTOC | `0x042A0` |
| LINKS | `0x042A4` |
| AUTOC2 | `0x042A8` |
| AUTOC3 | `0x042AC` |
| ANLP1 | `0x042B0` |
| MMNGC | `0x042D0` |
| LINKS2 | `0x04324` |
| MANC | `0x05820` |
| EERD | `0x10014` |
| SWSM | `0x10140` |
| FWSM | `0x10148` |
| FACTPS | `0x10150` |
| GSSR / SW_FW_SYNC | `0x10160` |
| SB_IOSF_INDIRECT_CTRL | `0x11144` |
| SB_IOSF_INDIRECT_DATA | `0x11148` |
| FUSES0_GROUP(i) | `0x11158` + 4·i |
| NW_MNG_IF_SEL | `0x11178` |
| CORECTL | `0x14F00` |
| FLEX_MNG (X552 host interface RAM) | `0x15800`–`0x15EFC` |
| HICR (X552) | `0x15F00` |
| FWSTS (X552) | `0x15F0C` |
| I2CCTL (X552) | `0x15F5C` |

### A.2 Bits

| Register | Bit(s) | Mask | Name |
|---|---|---|---|
| CTRL | 3 | `0x0000_0008` | LNK_RST |
| CTRL | 26 | `0x0400_0000` | RST |
| STATUS | 3:2 | `0x0000_000C` | LAN_ID |
| ESDP | 0–7 | `0x01`…`0x80` | SDP0–SDP7 data |
| ESDP | 8–15 | `0x0100`…`0x8000` | SDP0–SDP7 direction (1 = output) |
| ESDP | 16, 17 | `0x0001_0000`, `0x0002_0000` | SDP0, SDP1 native mode |
| HLREG0 | 16 | `0x0001_0000` | MDCSPD |
| MMNGC | 0 | `0x0000_0001` | MNG_VETO |
| FWSM | 3:1 | `0x0000_000E` | mode; `0x4` = pass-through |
| MANC | 17 | `0x0002_0000` | RCV_TCO_EN |
| FACTPS | 29 | `0x2000_0000` | MNGCG |
| FACTPS | 30 | `0x4000_0000` | LFS |
| SWSM | 0 | `0x1` | SMBI |
| SWSM | 1 | `0x2` | SWESMBI |
| SW_FW_SYNC | 31 | `0x8000_0000` | REGSMP |

(MSCA, MSRWD, AUTOC, AUTOC2, LINKS, I2CCTL, IOSF, KRM and NW_MNG_IF_SEL fields
are tabled in sections 2.1, 5.1, 5.2, 8.1, 3.1, 7.2, 7.3 and 7.6.)

### A.3 NVM words (16-bit word offsets)

| Word | Name | Use |
|---|---|---|
| `0x0001` | Control word 2 | bit 1 (`CCD`) is the D3 link-disable policy (not for boot) |
| `0x000F` | Firmware module pointer | LESM (5.3), SFI firmware version (5.12) |
| fw + `0x02` | LESM parameters pointer | LESM |
| lesm + `0x01` | LESM state 1 | bit 15 = enabled |
| fw + `0x04` | Pass-through patch configuration pointer | firmware version |
| ptp + `0x07` | Patch version | must be > 5 for SFI |
| `0x002B` | SFP PHY-init list pointer (82599) | 5.5 |
| `0x002C` | Device capabilities | bit 0 allow any SFP; bit 7 no crosstalk workaround |

### A.4 MDIO devices

| MMD | Number |
|---|---|
| PMA/PMD | 1 |
| PCS | 3 |
| PHY XS | 4 |
| Auto-negotiation | 7 |
| Vendor specific 1 | 30 (`0x1E`) |
| (CS4223/CS4227 on X553 MDIO) | 0 |

### A.5 I2C addresses (8-bit form)

| Device | Address |
|---|---|
| SFP ID EEPROM | `0xA0` |
| SFP diagnostics (SFF-8472) | `0xA2` |
| CS4227 retimer | `0xBE` |
| Port expander (X552 SFP) | `0xE0` |
| Thermal sensor (defined, unused here) | `0xF8` |

### A.6 PHY IDs

See 2.5. Compare after masking the revision: `id & 0xFFFF_FFF0`.

### A.7 sfp_type values

See 4.2 (0–18, `0xFFFE` not present, `0xFFFF` unknown). Values 0–2 are
82598-only.

### A.8 Shared-code speed flags (for cross-reference only)

| Flag | Value |
|---|---|
| 10M | `0x0002` |
| 100M | `0x0008` |
| 1G | `0x0020` |
| 10G | `0x0080` |
| 2.5G | `0x0400` |
| 5G | `0x0800` |

---

## Appendix B. Glossary

- **AN:** auto-negotiation. Clause 37 is 1G; clause 73 is the backplane
  KR/KX4/KX version; the 10GBASE-T version is in the PHY's MMD 7.
- **AUTOC / AUTOC2:** the 82599's link-mode registers.
- **CS4227:** a dual-port 10G retimer (Cortina/Inphi) between the X552 KR
  lanes and the SFP+ cages, managed over I2C.
- **DA:** direct-attach copper cable. Passive DA is "linear"; active DA with
  limiting electronics is treated like optics.
- **EDC:** electronic dispersion compensation mode of the CS4227 line side.
- **ESDP:** Extended SDP control register (the software-definable pins).
- **GSSR / SW_FW_SYNC:** the software/firmware semaphore register.
- **IOSF sideband:** Intel on-chip fabric sideband. On the X552 it is the
  indirect path from CSRs to the internal KR PHY registers.
- **iXFI:** the X552's forced-speed XFI mode between the internal KR PHY and
  the X557, used without AN.
- **KR / KX4 / KX:** 10GBASE-KR (1 lane, 10.3125 GBd), 10GBASE-KX4 (4 lanes)
  and 1000BASE-KX backplane PHYs.
- **LASI:** Link Alarm Status Interrupt, the clause-45 alarm mechanism.
- **LESM:** Link Establishment State Machine, 82599 firmware that manages the
  link itself. When it is enabled, AUTOC writes need the MAC_CSR semaphore.
- **LMS:** AUTOC link mode select (bits 15:13).
- **LPLU:** low-power link-up, for D3 and Wake-on-LAN.
- **MMD:** MDIO manageable device (clause 45 device address).
- **MNG / manageability:** the NIC's embedded firmware serving a BMC
  (NC-SI/SMBus pass-through).
- **Multispeed fiber:** a module that can run 10G or 1G, where the driver
  tries 10G first and falls back to 1G in software.
- **Pipeline reset:** the 82599 AUTOC write that toggles LMS bit 2 with
  Restart_AN so the link restarts in every mode.
- **SDP:** software-definable pin.
- **SFI:** the 10G serial electrical interface to an SFP+ module.
- **SmartSpeed:** 82599 fallback that drops KR from the advertisement after
  repeated failure.
- **X557:** Intel's external 10GBASE-T PHY used with the X552 (ID
  `0x0154024x`/`025x`).

---

## Appendix C. Source index

All paths are relative to FreeBSD `sys/dev/ixgbe/` at commit `669cd0d90ee7`.
The files are BSD-3-Clause, © Intel Corporation.

| Section | File: functions |
|---|---|
| 1.2 | `ixgbe_api.c`: `ixgbe_set_mac_type`; `ixgbe_82599.c`: `ixgbe_get_media_type_82599`; `ixgbe_x550.c`: `ixgbe_get_media_type_X550em` |
| 1.3 | `ixgbe_common.c`: `ixgbe_set_lan_id_multi_port_pcie` |
| 1.4 | `ixgbe_common.c`: `ixgbe_acquire_swfw_sync`, `ixgbe_release_swfw_sync`, `ixgbe_get_eeprom_semaphore`; `ixgbe_x540.c`: `ixgbe_acquire_swfw_sync_X540`, `ixgbe_release_swfw_sync_X540`, `ixgbe_init_swfw_sync_X540`; `ixgbe_x550.c`: `ixgbe_acquire_swfw_sync_X550em` |
| 1.5 | `ixgbe_phy.c`: `ixgbe_check_reset_blocked`; `ixgbe_common.c`: `ixgbe_mng_present`, `ixgbe_mng_enabled` |
| 2 | `ixgbe_phy.c`: `ixgbe_read_phy_reg_mdi`, `ixgbe_write_phy_reg_mdi`, `ixgbe_identify_phy_generic`, `ixgbe_probe_phy`, `ixgbe_get_phy_type_from_id`, `ixgbe_reset_phy_generic`; `if_ix_mdio_hw.c`: clause-22 helpers; `ixgbe_x550.c`: `ixgbe_set_mdio_speed` |
| 3 | `ixgbe_phy.c`: I2C primitives, `ixgbe_read_i2c_byte_generic_int`, `ixgbe_read_i2c_combined_generic_int`, `ixgbe_write_i2c_combined_generic_int`; `ixgbe_82599.c`: `ixgbe_read_i2c_byte_82599` |
| 4 | `ixgbe_phy.c`: `ixgbe_identify_sfp_module_generic`, `ixgbe_identify_qsfp_module_generic`; `ixgbe_common.c`: `ixgbe_set_soft_rate_select_speed`; `ixgbe_82599.c`: `ixgbe_set_hard_rate_select_speed` |
| 5 | `ixgbe_82599.c`: `ixgbe_setup_sfp_modules_82599`, `prot_autoc_write_82599`, `ixgbe_reset_pipeline_82599`, `ixgbe_get_link_capabilities_82599`, `ixgbe_setup_mac_link_82599`, `ixgbe_setup_mac_link_smartspeed`, `ixgbe_reset_hw_82599`, `ixgbe_verify_lesm_fw_enabled_82599`; `ixgbe_phy.c`: `ixgbe_get_sfp_init_sequence_offsets`; `ixgbe_common.c`: `ixgbe_setup_mac_link_multispeed_fiber` |
| 6 | `ixgbe_x540.c`: `ixgbe_init_ops_X540`, `ixgbe_reset_hw_X540`; `ixgbe_phy.c`: `ixgbe_setup_phy_link_generic`, `ixgbe_set_copper_phy_power` |
| 7 | `ixgbe_x550.c`: IOSF, KR, iXFI, CS4227, X557 and init functions as cited per subsection |
| 8 | `ixgbe_common.c`: `ixgbe_check_mac_link_generic`; `ixgbe_x550.c`: `ixgbe_check_link_t_X550em` |
| 9 | `if_ix.c`: `ixgbe_if_attach_pre`, `ixgbe_config_link`, `ixgbe_handle_mod`, `ixgbe_handle_msf`, `ixgbe_handle_phy` (call order) |
| 11 | `ixgbe_82599.c`: `ixgbe_get_media_type_82599`, `ixgbe_init_mac_link_ops_82599`, `ixgbe_get_link_capabilities_82599`; `ixgbe_x550.c`: `ixgbe_read_ee_hostif_X550`; `ixgbe_common.c`: `ixgbe_hic_unlocked`, `ixgbe_start_hw_generic`, `ixgbe_need_crosstalk_fix`; `ixgbe_phy.c`: `ixgbe_identify_qsfp_module_generic` (commit `32b8381d711c`) |

Intel's BSD-3-Clause notice for the shared code whose register layouts and
sequences this document describes is reproduced in the repository `NOTICE`
file.
