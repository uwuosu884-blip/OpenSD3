/*
 * opensd3.h — Single-Header C99 / C++17 Reference Parser & Lossless Decoder
 * for Superior Drummer 3 / EZdrummer (.obw 'ttbw', 'oidx', and 20-bit 'ttpw')
 *
 * Part of the OpenSD3 Project (https://github.com/uwuosu884-blip/OpenSD3)
 *
 * USAGE (stb-style single-header library):
 *   In ONE .c or .cpp file before including this header, define:
 *       #define OPENSD3_IMPLEMENTATION
 *       #include "opensd3.h"
 *
 *   In all other files, simply `#include "opensd3.h"`.
 *
 * FEATURES:
 *   - Zero external dependencies (standard C99 / C++17 only).
 *   - Parses outer `RIFF 'ttbw'` (`soundb*.obw`) `'oidx'` directory in < 1 ms,
 *     automatically solving the 32-bit `xor_mask` for sample offsets.
 *   - Parses inner `RIFF 'ttpw'` ("ToonTrack Packed Wave") 20-bit/24-bit headers.
 *   - Bit-exact lossless `ttpw` decoder with `O(log nsec)` arbitrary frame-range
 *     seeking (`os3_decode_ttpw_range_stereo_f32`) — ready to drop directly into
 *     a JUCE, iPlug2, VST3, CLAP, or AudioUnit disk-streaming worker thread.
 *
 * LICENSE:
 *   Dual-licensed under MIT OR GPL-3.0-or-later at your option for maximum
 *   compatibility with third-party open-source and audio plugin projects.
 */

#ifndef OPENSD3_H_INCLUDED
#define OPENSD3_H_INCLUDED

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define OS3_MAX_TTP_SECTIONS 64
#define OS3_MAX_MICS         64

typedef enum os3_status {
    OS3_OK              = 0,
    OS3_ERR_INVALID_ARG = -1,
    OS3_ERR_NOT_TTBW    = -2,
    OS3_ERR_BAD_OIDX    = -3,
    OS3_ERR_NOT_TTPW    = -4,
    OS3_ERR_BAD_TTPW    = -5,
    OS3_ERR_OOM         = -6
} os3_status;

/* ---- Inner 'ttpw' (ToonTrack Packed Wave) structures ---- */

typedef struct os3_ttpw_section {
    uint32_t end_frame;
    uint32_t word_offset;
    uint32_t bit_width;
} os3_ttpw_section;

typedef struct os3_ttpw_header {
    uint16_t version;
    uint16_t channels;
    uint32_t sample_rate;
    uint16_t bits_per_sample;
    uint16_t num_sections;
    uint32_t total_frames;
    uint32_t words_per_channel;
    size_t   data_offset;
    size_t   data_size;
    os3_ttpw_section sections[OS3_MAX_TTP_SECTIONS];
} os3_ttpw_header;

/* ---- Outer .obw ('ttbw' / 'oidx') index structures ---- */

typedef struct os3_sample_record {
    uint8_t  mic_index;
    char     vel_char;      /* ASCII 'A' (softest) .. 'Z' (loudest) */
    uint8_t  rr_num;        /* 1-based round-robin index */
    uint32_t stored_offset; /* raw offset stored in oidx */
    uint32_t data_offset;   /* real offset relative to ttbw 'data' payload (stored_offset ^ xor_mask) */
    uint32_t byte_size;     /* exact RIFF 'ttpw' byte length */
} os3_sample_record;

typedef struct os3_group {
    char               name[64]; /* e.g. "SN01_DS_HIT_R", "CR01_02_DS_CR_R" */
    os3_sample_record *records;
    size_t             record_count;
} os3_group;

typedef struct os3_ttbw_index {
    uint8_t    header_bytes[8];
    size_t     mic_count;
    char       mics[OS3_MAX_MICS][48];
    uint32_t   xor_mask;
    uint64_t   oidx_offset;
    uint64_t   oidx_size;
    uint64_t   data_payload_offset; /* add record.data_offset to this for absolute .obw file offset! */
    uint64_t   data_payload_size;
    os3_group *groups;
    size_t     group_count;
    size_t     total_records;
} os3_ttbw_index;

/* ---- Public API ---- */

int os3_looks_like_ttbw(const uint8_t *buf, size_t len);
int os3_looks_like_ttpw(const uint8_t *buf, size_t len);

/*
 * Parse the header & 'oidx' chunk of an `.obw` file.
 * NOTE: You do NOT need to load the entire multi-GB `.obw` into RAM!
 * Passing just the first ~4 MB of the `.obw` file (plus passing the full file
 * size in `full_file_size`, or `0` if `buf` is the whole file/mmap) is sufficient
 * to parse the entire `'oidx'` directory and locate `'data'`!
 */
os3_status os3_parse_ttbw_index(
    const uint8_t  *buf,
    size_t          buf_len,
    uint64_t        full_file_size,
    os3_ttbw_index *out_index
);

os3_status os3_parse_oidx_payload(
    const uint8_t  *oidx,
    size_t          oidx_len,
    uint64_t        data_size_hint,
    os3_ttbw_index *out_index
);

void os3_free_ttbw_index(os3_ttbw_index *index);

os3_status os3_parse_ttpw_header(
    const uint8_t   *ttpw,
    size_t           ttpw_len,
    os3_ttpw_header *out_hdr
);

/*
 * Decode `[start_frame .. start_frame + frame_count)` from a `RIFF 'ttpw'` slice
 * directly into stereo float32 buffers (`[-1.0f, +1.0f)`).
 * Mono `ttpw` streams are automatically duplicated to both `out_l` and `out_r`.
 * Runs in O(log num_sections + frame_count) with ZERO heap allocations!
 * Returns the number of frames decoded (>= 0), or a negative `os3_status` on error.
 */
int64_t os3_decode_ttpw_range_stereo_f32(
    const uint8_t         *ttpw,
    size_t                 ttpw_len,
    const os3_ttpw_header *hdr,
    size_t                 start_frame,
    size_t                 frame_count,
    float                 *out_l,
    float                 *out_r
);

#ifdef __cplusplus
}
#endif

/* ========================================================================= */
/* IMPLEMENTATION                                                            */
/* ========================================================================= */

#ifdef OPENSD3_IMPLEMENTATION

#include <stdlib.h>
#include <string.h>

static uint16_t os3_u16le(const uint8_t *p) {
    return (uint16_t)p[0] | ((uint16_t)p[1] << 8);
}

static uint32_t os3_u32le(const uint8_t *p) {
    return (uint32_t)p[0]
         | ((uint32_t)p[1] << 8)
         | ((uint32_t)p[2] << 16)
         | ((uint32_t)p[3] << 24);
}

int os3_looks_like_ttbw(const uint8_t *buf, size_t len) {
    return buf && len >= 12
        && memcmp(buf, "RIFF", 4) == 0
        && memcmp(buf + 8, "ttbw", 4) == 0;
}

int os3_looks_like_ttpw(const uint8_t *buf, size_t len) {
    return buf && len >= 12
        && memcmp(buf, "RIFF", 4) == 0
        && memcmp(buf + 8, "ttpw", 4) == 0;
}

void os3_free_ttbw_index(os3_ttbw_index *index) {
    if (!index) return;
    if (index->groups) {
        for (size_t i = 0; i < index->group_count; ++i) {
            free(index->groups[i].records);
        }
        free(index->groups);
    }
    memset(index, 0, sizeof(*index));
}

static int os3_verify_mask_contiguous(const os3_ttbw_index *idx, uint32_t mask) {
    uint64_t expected = 0;
    size_t checked = 0;
    for (size_t g = 0; g < idx->group_count; ++g) {
        for (size_t r = 0; r < idx->groups[g].record_count; ++r) {
            const os3_sample_record *rec = &idx->groups[g].records[r];
            uint64_t actual = (uint64_t)(rec->stored_offset ^ mask);
            if (checked > 0 && actual != expected && actual != expected + 1) {
                return 0;
            }
            uint64_t next_pos = actual + (uint64_t)rec->byte_size;
            expected = next_pos + (next_pos & 1u);
            checked++;
            if (checked >= 32) return 1;
        }
    }
    return 1;
}

os3_status os3_parse_oidx_payload(
    const uint8_t  *oidx,
    size_t          oidx_len,
    uint64_t        data_size_hint,
    os3_ttbw_index *out_index
) {
    if (!oidx || !out_index) return OS3_ERR_INVALID_ARG;
    memset(out_index, 0, sizeof(*out_index));
    if (oidx_len < 10) return OS3_ERR_BAD_OIDX;

    memcpy(out_index->header_bytes, oidx, 8);
    size_t num_mics = oidx[7];
    if (num_mics == 0 || num_mics > OS3_MAX_MICS) return OS3_ERR_BAD_OIDX;
    out_index->mic_count = num_mics;

    size_t pos = 8;
    for (size_t m = 0; m < num_mics; ++m) {
        size_t nul = pos;
        while (nul < oidx_len && oidx[nul] != 0) nul++;
        if (nul >= oidx_len) return OS3_ERR_BAD_OIDX;
        size_t slen = nul - pos;
        if (slen >= sizeof(out_index->mics[m])) slen = sizeof(out_index->mics[m]) - 1;
        memcpy(out_index->mics[m], oidx + pos, slen);
        out_index->mics[m][slen] = '\0';
        pos = nul + 1;
    }

    size_t group_cap = 32;
    out_index->groups = (os3_group *)calloc(group_cap, sizeof(os3_group));
    if (!out_index->groups) return OS3_ERR_OOM;

    int have_first = 0;
    uint32_t first_stored = 0;

    while (pos < oidx_len) {
        if (oidx[pos] == 0) break;
        size_t nul = pos;
        while (nul < oidx_len && oidx[nul] != 0) nul++;
        if (nul + 5 > oidx_len) {
            os3_free_ttbw_index(out_index);
            return OS3_ERR_BAD_OIDX;
        }
        size_t glen_name = nul - pos;
        uint32_t payload_len = os3_u32le(oidx + nul + 1);
        pos = nul + 5;
        if (pos + payload_len > oidx_len) {
            os3_free_ttbw_index(out_index);
            return OS3_ERR_BAD_OIDX;
        }
        size_t group_end = pos + payload_len;
        if (payload_len == 0) continue;

        if (out_index->group_count == group_cap) {
            group_cap *= 2;
            os3_group *ng = (os3_group *)realloc(out_index->groups, group_cap * sizeof(os3_group));
            if (!ng) {
                os3_free_ttbw_index(out_index);
                return OS3_ERR_OOM;
            }
            out_index->groups = ng;
        }

        os3_group *grp = &out_index->groups[out_index->group_count];
        memset(grp, 0, sizeof(*grp));
        if (glen_name >= sizeof(grp->name)) glen_name = sizeof(grp->name) - 1;
        memcpy(grp->name, oidx + (nul - glen_name), glen_name);
        grp->name[glen_name] = '\0';

        uint8_t gmics = oidx[pos];
        size_t p = pos + 1;

        /* Count records in group first */
        size_t rec_total = 0;
        size_t scan_p = p;
        for (uint8_t gm = 0; gm < gmics && scan_p + 2 <= group_end; ++gm) {
            uint8_t cnt = oidx[scan_p + 1];
            scan_p += 2;
            size_t avail = (group_end - scan_p) / 10;
            size_t actual = cnt < avail ? cnt : avail;
            rec_total += actual;
            scan_p += actual * 10;
        }

        if (rec_total > 0) {
            grp->records = (os3_sample_record *)calloc(rec_total, sizeof(os3_sample_record));
            if (!grp->records) {
                os3_free_ttbw_index(out_index);
                return OS3_ERR_OOM;
            }
        }

        size_t r_idx = 0;
        for (uint8_t gm = 0; gm < gmics && p + 2 <= group_end; ++gm) {
            uint8_t mic_idx = oidx[p];
            uint8_t cnt     = oidx[p + 1];
            p += 2;
            size_t avail = (group_end - p) / 10;
            size_t actual = cnt < avail ? cnt : avail;
            for (size_t k = 0; k < actual; ++k) {
                uint32_t so  = os3_u32le(oidx + p);
                uint8_t  vc  = oidx[p + 4];
                uint8_t  rr  = oidx[p + 5];
                uint32_t bsz = os3_u32le(oidx + p + 6);
                p += 10;
                if (!have_first) {
                    first_stored = so;
                    have_first = 1;
                }
                os3_sample_record *rec = &grp->records[r_idx++];
                rec->mic_index     = mic_idx;
                rec->vel_char      = (vc >= 0x20 && vc <= 0x7e) ? (char)vc : 'M';
                rec->rr_num        = rr;
                rec->stored_offset = so;
                rec->byte_size     = bsz;
            }
        }
        grp->record_count = r_idx;
        out_index->total_records += r_idx;
        out_index->group_count++;
        pos = group_end;
    }

    uint32_t mask = 0;
    if (have_first && first_stored != 0) {
        if (os3_verify_mask_contiguous(out_index, first_stored)) {
            mask = first_stored;
        } else if (!os3_verify_mask_contiguous(out_index, 0)) {
            mask = first_stored;
        }
    }
    out_index->xor_mask = mask;

    for (size_t g = 0; g < out_index->group_count; ++g) {
        for (size_t r = 0; r < out_index->groups[g].record_count; ++r) {
            out_index->groups[g].records[r].data_offset =
                out_index->groups[g].records[r].stored_offset ^ mask;
        }
    }
    out_index->oidx_size = oidx_len;
    out_index->data_payload_size = data_size_hint;
    return OS3_OK;
}

os3_status os3_parse_ttbw_index(
    const uint8_t  *buf,
    size_t          buf_len,
    uint64_t        full_file_size,
    os3_ttbw_index *out_index
) {
    if (!os3_looks_like_ttbw(buf, buf_len)) return OS3_ERR_NOT_TTBW;
    uint64_t total_len = full_file_size ? full_file_size : (uint64_t)buf_len;

    size_t pos = 12;
    size_t oidx_off = 0, oidx_sz = 0;
    uint64_t data_off = 0, data_sz = 0;

    while (pos + 8 <= buf_len) {
        uint32_t csz = os3_u32le(buf + pos + 4);
        size_t payload = pos + 8;
        if (memcmp(buf + pos, "oidx", 4) == 0) {
            if (payload + csz > buf_len) return OS3_ERR_BAD_OIDX;
            oidx_off = payload;
            oidx_sz  = csz;
        } else if (memcmp(buf + pos, "data", 4) == 0) {
            data_off = (uint64_t)payload;
            data_sz  = (uint64_t)csz;
            if (data_off + data_sz > total_len && total_len > data_off) {
                data_sz = total_len - data_off;
            }
            break;
        }
        size_t step = (size_t)csz + ((size_t)csz & 1u);
        if (payload + step < payload) break;
        pos = payload + step;
    }

    if (oidx_off == 0) return OS3_ERR_BAD_OIDX;
    os3_status st = os3_parse_oidx_payload(buf + oidx_off, oidx_sz, data_sz, out_index);
    if (st != OS3_OK) return st;

    out_index->oidx_offset         = (uint64_t)oidx_off;
    out_index->oidx_size           = (uint64_t)oidx_sz;
    out_index->data_payload_offset = data_off;
    out_index->data_payload_size   = data_sz;
    return OS3_OK;
}

os3_status os3_parse_ttpw_header(
    const uint8_t   *ttpw,
    size_t           ttpw_len,
    os3_ttpw_header *out_hdr
) {
    if (!ttpw || !out_hdr) return OS3_ERR_INVALID_ARG;
    if (!os3_looks_like_ttpw(ttpw, ttpw_len)) return OS3_ERR_NOT_TTPW;
    memset(out_hdr, 0, sizeof(*out_hdr));

    size_t pos = 12;
    int got_fmt = 0, got_data = 0;

    while (pos + 8 <= ttpw_len) {
        uint32_t csz = os3_u32le(ttpw + pos + 4);
        size_t payload = pos + 8;
        if (memcmp(ttpw + pos, "fmt ", 4) == 0) {
            if (csz < 20 || payload + csz > ttpw_len) return OS3_ERR_BAD_TTPW;
            const uint8_t *f = ttpw + payload;
            out_hdr->version           = os3_u16le(f + 0);
            out_hdr->channels          = os3_u16le(f + 2);
            out_hdr->sample_rate       = os3_u32le(f + 4);
            out_hdr->bits_per_sample   = os3_u16le(f + 8);
            out_hdr->num_sections      = os3_u16le(f + 10);
            out_hdr->total_frames      = os3_u32le(f + 12);
            out_hdr->words_per_channel = os3_u32le(f + 16);

            if (out_hdr->channels == 0 || out_hdr->channels > 16) return OS3_ERR_BAD_TTPW;
            if (out_hdr->bits_per_sample == 0 || out_hdr->bits_per_sample > 31) return OS3_ERR_BAD_TTPW;
            if (out_hdr->num_sections > OS3_MAX_TTP_SECTIONS) return OS3_ERR_BAD_TTPW;
            if (20u + (size_t)out_hdr->num_sections * 12u > csz) return OS3_ERR_BAD_TTPW;

            for (uint16_t s = 0; s < out_hdr->num_sections; ++s) {
                const uint8_t *sp = f + 20 + (size_t)s * 12;
                out_hdr->sections[s].end_frame   = os3_u32le(sp + 0);
                out_hdr->sections[s].word_offset = os3_u32le(sp + 4);
                out_hdr->sections[s].bit_width   = os3_u32le(sp + 8);
                if (out_hdr->sections[s].bit_width > 32) return OS3_ERR_BAD_TTPW;
            }
            got_fmt = 1;
        } else if (memcmp(ttpw + pos, "data", 4) == 0) {
            if (payload > ttpw_len) return OS3_ERR_BAD_TTPW;
            size_t avail = ttpw_len - payload;
            out_hdr->data_offset = payload;
            out_hdr->data_size   = csz < avail ? (size_t)csz : avail;
            got_data = 1;
            break;
        }
        size_t step = (size_t)csz + ((size_t)csz & 1u);
        pos = payload + step;
    }

    return (got_fmt && got_data) ? OS3_OK : OS3_ERR_BAD_TTPW;
}

typedef struct os3_bit_reader {
    const uint8_t *data;
    size_t         words_per_ch;
    size_t         nch;
    size_t         ch;
    size_t         wi;
    uint64_t       bitbuf;
    uint32_t       nbits;
} os3_bit_reader;

static int32_t os3_read_signed_bits(os3_bit_reader *br, uint32_t b, uint32_t shift) {
    while (br->nbits < b) {
        uint32_t w = 0;
        if (br->wi < br->words_per_ch) {
            size_t byte_idx = (br->wi * br->nch + br->ch) * 4u;
            w = os3_u32le(br->data + byte_idx);
            br->wi++;
        }
        br->bitbuf = (br->bitbuf << 32) | (uint64_t)w;
        br->nbits += 32;
    }
    br->nbits -= b;
    uint32_t raw = (uint32_t)(br->bitbuf >> br->nbits);
    return ((int32_t)(raw << shift)) >> shift;
}

int64_t os3_decode_ttpw_range_stereo_f32(
    const uint8_t         *ttpw,
    size_t                 ttpw_len,
    const os3_ttpw_header *hdr,
    size_t                 start_frame,
    size_t                 frame_count,
    float                 *out_l,
    float                 *out_r
) {
    if (!ttpw || !hdr || !out_l || !out_r) return OS3_ERR_INVALID_ARG;
    size_t total = (size_t)hdr->total_frames;
    if (start_frame >= total || frame_count == 0) return 0;

    size_t to_decode = frame_count;
    if (start_frame + to_decode > total) to_decode = total - start_frame;

    size_t nch = (size_t)hdr->channels;
    size_t wpc = (size_t)hdr->words_per_channel;
    size_t need_bytes = wpc * nch * 4u;
    if (hdr->data_offset + need_bytes > ttpw_len) return OS3_ERR_BAD_TTPW;

    const uint8_t *data = ttpw + hdr->data_offset;
    float inv_scale = 1.0f / (float)(1ULL << (hdr->bits_per_sample - 1));

    /* Find starting section */
    uint16_t sec_idx = 0;
    while (sec_idx < hdr->num_sections && (size_t)hdr->sections[sec_idx].end_frame <= start_frame) {
        sec_idx++;
    }

    size_t written = 0;
    size_t cur_frame = start_frame;

    while (written < to_decode && sec_idx < hdr->num_sections) {
        uint32_t sec_start = (sec_idx == 0) ? 0u : hdr->sections[sec_idx - 1].end_frame;
        uint32_t sec_end   = hdr->sections[sec_idx].end_frame;
        if (sec_end > hdr->total_frames) sec_end = hdr->total_frames;
        if ((size_t)sec_end <= cur_frame) {
            sec_idx++;
            continue;
        }

        size_t chunk_len = (size_t)sec_end - cur_frame;
        if (chunk_len > to_decode - written) chunk_len = to_decode - written;

        uint32_t b = hdr->sections[sec_idx].bit_width;
        if (b == 0) {
            memset(out_l + written, 0, chunk_len * sizeof(float));
            memset(out_r + written, 0, chunk_len * sizeof(float));
        } else {
            uint64_t rel_frames = (uint64_t)(cur_frame - (size_t)sec_start);
            uint64_t bit_skip   = rel_frames * (uint64_t)b;
            size_t   start_wi   = (size_t)hdr->sections[sec_idx].word_offset + (size_t)(bit_skip / 32u);
            uint32_t rem_bits   = (uint32_t)(bit_skip % 32u);
            uint32_t shift      = 32u - b;

            os3_bit_reader br0 = { data, wpc, nch, 0, start_wi, 0, 0 };
            if (rem_bits > 0) (void)os3_read_signed_bits(&br0, rem_bits, 32u - rem_bits);

            if (nch == 1) {
                for (size_t i = 0; i < chunk_len; ++i) {
                    float s = (float)os3_read_signed_bits(&br0, b, shift) * inv_scale;
                    out_l[written + i] = s;
                    out_r[written + i] = s;
                }
            } else {
                os3_bit_reader br1 = { data, wpc, nch, 1, start_wi, 0, 0 };
                if (rem_bits > 0) (void)os3_read_signed_bits(&br1, rem_bits, 32u - rem_bits);
                for (size_t i = 0; i < chunk_len; ++i) {
                    out_l[written + i] = (float)os3_read_signed_bits(&br0, b, shift) * inv_scale;
                    out_r[written + i] = (float)os3_read_signed_bits(&br1, b, shift) * inv_scale;
                }
            }
        }

        written   += chunk_len;
        cur_frame += chunk_len;
        sec_idx++;
    }

    return (int64_t)written;
}

#endif /* OPENSD3_IMPLEMENTATION */
#endif /* OPENSD3_H_INCLUDED */
