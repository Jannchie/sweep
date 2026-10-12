//! MFT 文件记录的解析：更新序列修正、属性遍历、数据运行表、$STANDARD_INFORMATION 与 $FILE_NAME。
//! 全是对字节切片的纯函数，不碰磁盘，便于用构造出来的缓冲区做单元测试。
//!
//! 布局参考 NTFS 文档（flatcap.github.io/linux-ntfs）与 Windows 的 FILE_RECORD_SEGMENT_HEADER。

pub const ATTR_STANDARD_INFORMATION: u32 = 0x10;
pub const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
pub const ATTR_FILE_NAME: u32 = 0x30;
pub const ATTR_DATA: u32 = 0x80;
pub const ATTR_BITMAP: u32 = 0xB0;
pub const ATTR_END: u32 = 0xFFFF_FFFF;

/// 记录头 flags。
pub const RECORD_IN_USE: u16 = 0x0001;
pub const RECORD_IS_DIRECTORY: u16 = 0x0002;

/// $FILE_NAME 的命名空间。
pub const NS_POSIX: u8 = 0;
pub const NS_WIN32: u8 = 1;
pub const NS_DOS: u8 = 2;
pub const NS_WIN32_AND_DOS: u8 = 3;

/// 非常驻属性的 flags：压缩、稀疏时头部多出"实际分配"字段。
const ATTR_FLAG_COMPRESSED: u16 = 0x0001;
const ATTR_FLAG_SPARSE: u16 = 0x8000;

/// 文件引用的低 48 位是记录号，高 16 位是序列号。
pub fn ref_record(r: u64) -> u64 {
    r & 0x0000_FFFF_FFFF_FFFF
}

pub fn ref_seq(r: u64) -> u16 {
    (r >> 48) as u16
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    BadMagic,
    Truncated,
    /// 更新序列不匹配：记录写到一半（撕裂写入）或者已损坏。
    BadFixup,
    BadRuns,
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

/// 应用更新序列（fixup）：每个扇区最后两个字节写盘时被替换成了更新序列号，
/// 原值存在更新序列数组里。校验并还原。`rec` 是一整条记录。
pub fn apply_fixup(rec: &mut [u8]) -> Result<(), Error> {
    if rec.get(0..4) != Some(b"FILE") {
        return Err(Error::BadMagic);
    }
    let usa_off = u16_at(rec, 4).ok_or(Error::Truncated)? as usize;
    let usa_count = u16_at(rec, 6).ok_or(Error::Truncated)? as usize;
    // 数组第一个元素是序列号本身，之后每个扇区一个。
    if usa_count < 2 || usa_off + usa_count * 2 > rec.len() {
        return Err(Error::Truncated);
    }
    let stride = rec.len() / (usa_count - 1);
    if stride < 2 || stride * (usa_count - 1) != rec.len() {
        return Err(Error::BadFixup);
    }
    let usn = [rec[usa_off], rec[usa_off + 1]];
    for i in 1..usa_count {
        let end = i * stride;
        if rec[end - 2..end] != usn {
            return Err(Error::BadFixup);
        }
        let saved = [rec[usa_off + i * 2], rec[usa_off + i * 2 + 1]];
        rec[end - 2..end].copy_from_slice(&saved);
    }
    Ok(())
}

/// 记录头里用得到的字段。
#[derive(Debug, Clone, Copy)]
pub struct Header {
    pub seq: u16,
    pub flags: u16,
    pub first_attr: usize,
    pub used: usize,
    /// 扩展记录指向的基本记录引用；基本记录为 0。
    pub base: u64,
}

pub fn header(rec: &[u8]) -> Result<Header, Error> {
    let h = Header {
        seq: u16_at(rec, 0x10).ok_or(Error::Truncated)?,
        first_attr: u16_at(rec, 0x14).ok_or(Error::Truncated)? as usize,
        flags: u16_at(rec, 0x16).ok_or(Error::Truncated)?,
        used: u32_at(rec, 0x18).ok_or(Error::Truncated)? as usize,
        base: u64_at(rec, 0x20).ok_or(Error::Truncated)?,
    };
    if h.first_attr < 0x30 || h.first_attr >= rec.len() {
        return Err(Error::Truncated);
    }
    Ok(h)
}

/// 一个属性。`raw` 是整个属性（含头部）。
#[derive(Debug, Clone, Copy)]
pub struct Attr<'a> {
    pub type_code: u32,
    pub non_resident: bool,
    pub flags: u16,
    /// 有名字的属性（命名数据流等）。
    pub named: bool,
    raw: &'a [u8],
}

/// 非常驻属性头里的大小信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NonResident {
    pub lowest_vcn: u64,
    pub highest_vcn: u64,
    /// 分配的字节数（簇对齐）。
    pub allocated: u64,
    pub real: u64,
    /// 压缩、稀疏属性实际占用的字节；其他情况等于 `allocated`。
    pub on_disk: u64,
}

impl<'a> Attr<'a> {
    /// 常驻属性的值。
    pub fn value(&self) -> Option<&'a [u8]> {
        if self.non_resident {
            return None;
        }
        let len = u32_at(self.raw, 0x10)? as usize;
        let off = u16_at(self.raw, 0x14)? as usize;
        self.raw.get(off..off + len)
    }

    pub fn non_resident(&self) -> Option<NonResident> {
        if !self.non_resident {
            return None;
        }
        let allocated = u64_at(self.raw, 0x28)?;
        let compressed = self.flags & (ATTR_FLAG_COMPRESSED | ATTR_FLAG_SPARSE) != 0;
        Some(NonResident {
            lowest_vcn: u64_at(self.raw, 0x10)?,
            highest_vcn: u64_at(self.raw, 0x18)?,
            allocated,
            real: u64_at(self.raw, 0x30)?,
            // 只有 lowest_vcn 为 0 的那一段才带这个字段。
            on_disk: if compressed {
                u64_at(self.raw, 0x40).unwrap_or(allocated)
            } else {
                allocated
            },
        })
    }

    /// 非常驻属性的数据运行表（原始字节）。
    pub fn runs(&self) -> Option<&'a [u8]> {
        if !self.non_resident {
            return None;
        }
        let off = u16_at(self.raw, 0x20)? as usize;
        self.raw.get(off..)
    }

    /// 属性名（UTF-16），用来区分 $MFT 的 $DATA 与其他命名流。
    pub fn name_units(&self) -> Vec<u16> {
        let len = self.raw.get(9).copied().unwrap_or(0) as usize;
        let off = u16_at(self.raw, 0x0A).unwrap_or(0) as usize;
        self.raw
            .get(off..off + len * 2)
            .map(|b| {
                b.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&c| u16::from_le_bytes(c))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// 遍历记录里的属性，遇到结束标记或越界即停止（损坏的记录不会导致 panic）。
pub fn attributes<'a>(rec: &'a [u8], h: &Header) -> impl Iterator<Item = Attr<'a>> + 'a {
    let end = h.used.min(rec.len());
    let mut off = h.first_attr;
    std::iter::from_fn(move || {
        let type_code = u32_at(rec, off)?;
        if type_code == ATTR_END || off + 0x10 > end {
            return None;
        }
        let len = u32_at(rec, off + 4)? as usize;
        if len < 0x18 || off + len > end || !len.is_multiple_of(8) {
            return None;
        }
        let raw = &rec[off..off + len];
        off += len;
        Some(Attr {
            type_code,
            non_resident: raw[8] != 0,
            named: raw[9] != 0,
            flags: u16_at(raw, 0x0C).unwrap_or(0),
            raw,
        })
    })
}

/// 一段数据运行：起始逻辑簇号（稀疏段为 None）与簇数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub lcn: Option<u64>,
    pub clusters: u64,
}

/// 解码数据运行表。每段以一个字节开头：低 4 位是长度字段的字节数，高 4 位是偏移字段的字节数；
/// 偏移是相对上一段起点的有符号数，字节数为 0 表示稀疏段。以 0 字节结束。
pub fn decode_runs(mut b: &[u8]) -> Result<Vec<Run>, Error> {
    let mut runs = Vec::new();
    let mut lcn: i64 = 0;
    loop {
        let (&head, rest) = b.split_first().ok_or(Error::BadRuns)?;
        if head == 0 {
            return Ok(runs);
        }
        let (len_size, off_size) = ((head & 0x0F) as usize, (head >> 4) as usize);
        if len_size == 0 || len_size > 8 || off_size > 8 || rest.len() < len_size + off_size {
            return Err(Error::BadRuns);
        }
        let clusters = le_unsigned(&rest[..len_size]);
        let run = if off_size == 0 {
            Run {
                lcn: None,
                clusters,
            }
        } else {
            lcn = lcn.wrapping_add(le_signed(&rest[len_size..len_size + off_size]));
            if lcn < 0 {
                return Err(Error::BadRuns);
            }
            Run {
                lcn: Some(lcn as u64),
                clusters,
            }
        };
        runs.push(run);
        b = &rest[len_size + off_size..];
    }
}

fn le_unsigned(b: &[u8]) -> u64 {
    b.iter().rev().fold(0, |acc, &x| (acc << 8) | x as u64)
}

fn le_signed(b: &[u8]) -> i64 {
    let v = le_unsigned(b);
    let bits = b.len() * 8;
    if bits < 64 && v & (1 << (bits - 1)) != 0 {
        (v | (!0u64 << bits)) as i64
    } else {
        v as i64
    }
}

/// $STANDARD_INFORMATION 里用到的字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StdInfo {
    pub modified: u64,
    /// Win32 文件属性（隐藏、系统、重解析点等）。
    pub attributes: u32,
}

pub fn std_info(v: &[u8]) -> Option<StdInfo> {
    Some(StdInfo {
        modified: u64_at(v, 0x08)?,
        attributes: u32_at(v, 0x20)?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileName {
    pub parent: u64,
    pub namespace: u8,
    pub name: Vec<u16>,
}

pub fn file_name(v: &[u8]) -> Option<FileName> {
    let len = *v.get(0x40)? as usize;
    let namespace = *v.get(0x41)?;
    let name = v
        .get(0x42..0x42 + len * 2)?
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_le_bytes(c))
        .collect();
    Some(FileName {
        parent: u64_at(v, 0)?,
        namespace,
        name,
    })
}

/// $ATTRIBUTE_LIST 的一项：某个属性（的某一段）存放在哪条记录里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListEntry {
    pub type_code: u32,
    pub lowest_vcn: u64,
    pub record: u64,
    pub named: bool,
}

pub fn attribute_list(v: &[u8]) -> Vec<ListEntry> {
    let mut out = Vec::new();
    let mut off = 0;
    while let (Some(type_code), Some(len)) = (u32_at(v, off), u16_at(v, off + 4)) {
        if len < 0x1A || off + len as usize > v.len() {
            break;
        }
        out.push(ListEntry {
            type_code,
            named: v[off + 6] != 0,
            lowest_vcn: u64_at(v, off + 8).unwrap_or(0),
            record: ref_record(u64_at(v, off + 0x10).unwrap_or(0)),
        });
        off += len as usize;
    }
    out
}

/// 一条记录里解析出的、建树需要的全部信息。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub seq: u16,
    pub is_dir: bool,
    /// 扩展记录所属的基本记录号；基本记录为 None。
    pub base: Option<u64>,
    pub std: Option<StdInfo>,
    /// 非 DOS 命名空间的名字（每个硬链接一个）。
    pub names: Vec<FileName>,
    /// 这条记录里只有 8.3 短名时的短名。长名可能在同一文件的另一条（扩展）记录里，
    /// 所以不能在这里就把短名当作名字，交给建树时最后兜底。
    pub dos: Option<FileName>,
    /// 无名 $DATA 的 (逻辑大小, 实际占用)。只取起始段（lowest_vcn = 0）。
    pub data: Option<(u64, u64)>,
    /// 无名 $DATA 的实际占用（字节），与 `data` 的第二项相同；没有时为 0。
    pub streams_on_disk: u64,
    /// 命名 $DATA 流按运行表数出的已分配簇数（跳过稀疏段），用于元文件：
    /// $UsnJrnl:$J、$Secure:$SDS 的数据都在命名流里，$BadClus:$Bad 名义上和整个卷一样大，
    /// 实际全是稀疏段，而且属性头里不一定带稀疏标记，只能看运行表。
    pub named_clusters: u64,
}

/// 解析一条已做过 fixup 的记录。未使用的记录返回 None。
pub fn parse(rec: &[u8]) -> Result<Option<Parsed>, Error> {
    let h = header(rec)?;
    if h.flags & RECORD_IN_USE == 0 {
        return Ok(None);
    }
    let mut p = Parsed {
        seq: h.seq,
        is_dir: h.flags & RECORD_IS_DIRECTORY != 0,
        base: (ref_record(h.base) != 0).then(|| ref_record(h.base)),
        ..Default::default()
    };
    let mut dos = None;
    for a in attributes(rec, &h) {
        match a.type_code {
            ATTR_STANDARD_INFORMATION => p.std = a.value().and_then(std_info),
            ATTR_FILE_NAME => {
                if let Some(f) = a.value().and_then(file_name) {
                    if f.namespace == NS_DOS {
                        dos = Some(f);
                    } else if !p.names.contains(&f) {
                        p.names.push(f);
                    }
                }
            }
            ATTR_DATA => {
                if let Some(nr) = a.non_resident() {
                    if a.named {
                        let runs = decode_runs(a.runs().unwrap_or_default()).unwrap_or_default();
                        p.named_clusters += runs
                            .iter()
                            .filter(|r| r.lcn.is_some())
                            .map(|r| r.clusters)
                            .sum::<u64>();
                    } else if nr.lowest_vcn == 0 {
                        p.streams_on_disk = nr.on_disk;
                        p.data = Some((nr.real, nr.on_disk));
                    }
                } else if let Some(v) = a.value()
                    && !a.named
                {
                    // 常驻数据就在 MFT 记录里，不另占簇。目录枚举接口报告的"分配大小"是
                    // 按 8 字节对齐的值长度，这里与它保持一致，两种扫描方式的结果才能逐项对上。
                    p.data = Some((v.len() as u64, (v.len() as u64).div_ceil(8) * 8));
                }
            }
            _ => {}
        }
    }
    if p.names.is_empty() {
        p.dos = dos;
    }
    Ok(Some(p))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// 构造一条 1024 字节、扇区 512 的记录，属性依次写入；最后做"写盘"时的更新序列替换。
    pub struct RecordBuilder {
        buf: Vec<u8>,
        off: usize,
    }

    impl RecordBuilder {
        pub fn new(flags: u16, seq: u16, base: u64) -> Self {
            let mut buf = vec![0u8; 1024];
            buf[0..4].copy_from_slice(b"FILE");
            buf[4..6].copy_from_slice(&0x30u16.to_le_bytes()); // 更新序列数组偏移
            buf[6..8].copy_from_slice(&3u16.to_le_bytes()); // 1 + 2 个扇区
            buf[0x10..0x12].copy_from_slice(&seq.to_le_bytes());
            buf[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
            buf[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
            buf[0x1C..0x20].copy_from_slice(&1024u32.to_le_bytes());
            buf[0x20..0x28].copy_from_slice(&base.to_le_bytes());
            Self { buf, off: 0x38 }
        }

        fn push_attr(&mut self, mut attr: Vec<u8>) -> &mut Self {
            attr.resize(attr.len().div_ceil(8) * 8, 0);
            let len = attr.len() as u32;
            attr[4..8].copy_from_slice(&len.to_le_bytes());
            self.buf[self.off..self.off + attr.len()].copy_from_slice(&attr);
            self.off += attr.len();
            self
        }

        pub fn resident(&mut self, type_code: u32, name: &[u16], value: &[u8]) -> &mut Self {
            let name_off = 0x18usize;
            let value_off = (name_off + name.len() * 2).div_ceil(8) * 8;
            let mut a = vec![0u8; value_off + value.len()];
            a[0..4].copy_from_slice(&type_code.to_le_bytes());
            a[8] = 0;
            a[9] = name.len() as u8;
            a[0x0A..0x0C].copy_from_slice(&(name_off as u16).to_le_bytes());
            a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
            a[0x14..0x16].copy_from_slice(&(value_off as u16).to_le_bytes());
            for (i, u) in name.iter().enumerate() {
                a[name_off + i * 2..name_off + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
            }
            a[value_off..].copy_from_slice(value);
            self.push_attr(a)
        }

        #[allow(clippy::too_many_arguments)]
        pub fn non_resident(
            &mut self,
            type_code: u32,
            name: &[u16],
            flags: u16,
            lowest_vcn: u64,
            allocated: u64,
            real: u64,
            on_disk: u64,
            runs: &[u8],
        ) -> &mut Self {
            let name_off = 0x48usize;
            let runs_off = (name_off + name.len() * 2).div_ceil(8) * 8;
            let mut a = vec![0u8; runs_off + runs.len()];
            a[0..4].copy_from_slice(&type_code.to_le_bytes());
            a[8] = 1;
            a[9] = name.len() as u8;
            a[0x0A..0x0C].copy_from_slice(&(name_off as u16).to_le_bytes());
            a[0x0C..0x0E].copy_from_slice(&flags.to_le_bytes());
            a[0x10..0x18].copy_from_slice(&lowest_vcn.to_le_bytes());
            a[0x20..0x22].copy_from_slice(&(runs_off as u16).to_le_bytes());
            a[0x28..0x30].copy_from_slice(&allocated.to_le_bytes());
            a[0x30..0x38].copy_from_slice(&real.to_le_bytes());
            a[0x38..0x40].copy_from_slice(&real.to_le_bytes());
            a[0x40..0x48].copy_from_slice(&on_disk.to_le_bytes());
            for (i, u) in name.iter().enumerate() {
                a[name_off + i * 2..name_off + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
            }
            a[runs_off..].copy_from_slice(runs);
            self.push_attr(a)
        }

        pub fn std_info(&mut self, modified: u64, attributes: u32) -> &mut Self {
            let mut v = vec![0u8; 0x48];
            v[0x08..0x10].copy_from_slice(&modified.to_le_bytes());
            v[0x20..0x24].copy_from_slice(&attributes.to_le_bytes());
            self.resident(ATTR_STANDARD_INFORMATION, &[], &v)
        }

        pub fn file_name(&mut self, parent: u64, namespace: u8, name: &str) -> &mut Self {
            let units: Vec<u16> = name.encode_utf16().collect();
            let mut v = vec![0u8; 0x42 + units.len() * 2];
            v[0..8].copy_from_slice(&parent.to_le_bytes());
            v[0x40] = units.len() as u8;
            v[0x41] = namespace;
            for (i, u) in units.iter().enumerate() {
                v[0x42 + i * 2..0x44 + i * 2].copy_from_slice(&u.to_le_bytes());
            }
            self.resident(ATTR_FILE_NAME, &[], &v)
        }

        /// 写结束标记、已用长度，并模拟写盘：每个扇区末尾两字节换成更新序列号。
        pub fn build(&mut self) -> Vec<u8> {
            let mut b = self.buf.clone();
            b[self.off..self.off + 4].copy_from_slice(&ATTR_END.to_le_bytes());
            let used = (self.off + 8) as u32;
            b[0x18..0x1C].copy_from_slice(&used.to_le_bytes());
            let usn = [0x07, 0x00];
            b[0x30..0x32].copy_from_slice(&usn);
            for i in 1..3 {
                let end = i * 512;
                let saved = [b[end - 2], b[end - 1]];
                b[0x30 + i * 2..0x32 + i * 2].copy_from_slice(&saved);
                b[end - 2..end].copy_from_slice(&usn);
            }
            b
        }
    }

    fn file_ref(record: u64, seq: u16) -> u64 {
        record | (seq as u64) << 48
    }

    #[test]
    fn fixup_restores_sector_tails() {
        let mut rb = RecordBuilder::new(RECORD_IN_USE, 1, 0);
        // 一个跨过第一个扇区末尾的属性，末尾两个字节不是 0。
        let value: Vec<u8> = (0..600u32).map(|i| (i % 251) as u8 + 1).collect();
        rb.resident(ATTR_DATA, &[], &value);
        let mut rec = rb.build();
        assert_eq!(&rec[510..512], &[0x07, 0x00]);
        apply_fixup(&mut rec).unwrap();
        let h = header(&rec).unwrap();
        let a = attributes(&rec, &h).next().unwrap();
        assert_eq!(a.value().unwrap(), &value[..]);
    }

    #[test]
    fn fixup_detects_torn_writes() {
        let mut rec = RecordBuilder::new(RECORD_IN_USE, 1, 0).build();
        rec[1022] = 0x99;
        assert_eq!(apply_fixup(&mut rec), Err(Error::BadFixup));
        let mut junk = vec![0u8; 1024];
        assert_eq!(apply_fixup(&mut junk), Err(Error::BadMagic));
    }

    #[test]
    fn decodes_run_lists() {
        // 0x21: 长度 1 字节、偏移 2 字节。第二段偏移为负（-0x10），第三段是稀疏段。
        let runs = [
            0x21, 0x18, 0x34, 0x12, // 0x18 簇 @ 0x1234
            0x21, 0x08, 0xF0, 0xFF, // 8 簇 @ 0x1234 - 0x10
            0x01, 0x04, // 4 簇稀疏
            0x31, 0x02, 0x00, 0x00, 0x01, // 2 簇 @ +0x10000
            0x00,
        ];
        assert_eq!(
            decode_runs(&runs).unwrap(),
            vec![
                Run {
                    lcn: Some(0x1234),
                    clusters: 0x18
                },
                Run {
                    lcn: Some(0x1224),
                    clusters: 8
                },
                Run {
                    lcn: None,
                    clusters: 4
                },
                Run {
                    lcn: Some(0x11224),
                    clusters: 2
                },
            ]
        );
        assert_eq!(decode_runs(&[0x21, 0x01]), Err(Error::BadRuns));
        assert_eq!(decode_runs(&[]), Err(Error::BadRuns));
        // 第一段就是负偏移：非法。
        assert_eq!(decode_runs(&[0x11, 0x01, 0xFF, 0x00]), Err(Error::BadRuns));
    }

    #[test]
    fn parses_names_sizes_and_times() {
        let stream: Vec<u16> = "Zone.Identifier".encode_utf16().collect();
        let rec = RecordBuilder::new(RECORD_IN_USE, 3, 0)
            .std_info(0x01D0_0000_0000_0000, 0x22)
            .file_name(file_ref(5, 5), NS_DOS, "LONGFI~1.TXT")
            .file_name(file_ref(5, 5), NS_WIN32, "long file name.txt")
            .file_name(file_ref(77, 2), NS_POSIX, "hardlink.txt")
            .non_resident(
                ATTR_DATA,
                &[],
                0,
                0,
                8192,
                5000,
                8192,
                &[0x11, 0x02, 0x20, 0x00],
            )
            .resident(ATTR_DATA, &stream, b"[ZoneTransfer]")
            .build();
        let mut rec = rec;
        apply_fixup(&mut rec).unwrap();
        let p = parse(&rec).unwrap().unwrap();
        assert_eq!(p.seq, 3);
        assert!(!p.is_dir);
        assert_eq!(p.base, None);
        assert_eq!(
            p.std,
            Some(StdInfo {
                modified: 0x01D0_0000_0000_0000,
                attributes: 0x22
            })
        );
        let names: Vec<String> = p
            .names
            .iter()
            .map(|n| String::from_utf16_lossy(&n.name))
            .collect();
        assert_eq!(names, ["long file name.txt", "hardlink.txt"]);
        assert_eq!(ref_record(p.names[1].parent), 77);
        assert_eq!(ref_seq(p.names[1].parent), 2);
        assert_eq!(p.data, Some((5000, 8192)));
        assert_eq!((p.streams_on_disk, p.named_clusters), (8192, 0));
    }

    #[test]
    fn handles_resident_compressed_and_extension_records() {
        let mut rec = RecordBuilder::new(RECORD_IN_USE | RECORD_IS_DIRECTORY, 1, 0)
            .file_name(file_ref(5, 5), NS_WIN32_AND_DOS, "DIR")
            .build();
        apply_fixup(&mut rec).unwrap();
        let p = parse(&rec).unwrap().unwrap();
        assert!(p.is_dir && p.data.is_none());

        let mut rec = RecordBuilder::new(RECORD_IN_USE, 1, 0)
            .file_name(file_ref(5, 5), NS_WIN32, "tiny.txt")
            .resident(ATTR_DATA, &[], b"hello")
            .build();
        apply_fixup(&mut rec).unwrap();
        assert_eq!(parse(&rec).unwrap().unwrap().data, Some((5, 8)));

        // 压缩文件：逻辑 1 MB，分配 1 MB，实际只占 64 KB。
        let mut rec = RecordBuilder::new(RECORD_IN_USE, 1, 0)
            .non_resident(
                ATTR_DATA,
                &[],
                0x0001,
                0,
                1 << 20,
                1 << 20,
                1 << 16,
                &[0x00],
            )
            .build();
        apply_fixup(&mut rec).unwrap();
        assert_eq!(parse(&rec).unwrap().unwrap().data, Some((1 << 20, 1 << 16)));

        // 扩展记录：带着基本记录引用；lowest_vcn 不为 0 的 $DATA 段不提供大小。
        let mut rec = RecordBuilder::new(RECORD_IN_USE, 1, file_ref(42, 9))
            .non_resident(ATTR_DATA, &[], 0, 100, 0, 0, 0, &[0x00])
            .build();
        apply_fixup(&mut rec).unwrap();
        let p = parse(&rec).unwrap().unwrap();
        assert_eq!(p.base, Some(42));
        assert_eq!(p.data, None);

        let mut rec = RecordBuilder::new(0, 1, 0).build();
        apply_fixup(&mut rec).unwrap();
        assert_eq!(parse(&rec).unwrap(), None, "未使用的记录");
    }

    #[test]
    fn stops_at_corrupt_attribute_lengths() {
        let mut rec = RecordBuilder::new(RECORD_IN_USE, 1, 0)
            .file_name(file_ref(5, 5), NS_WIN32, "a")
            .build();
        apply_fixup(&mut rec).unwrap();
        // 把第一个属性的长度改成越界值。
        rec[0x38 + 4..0x38 + 8].copy_from_slice(&0xFFFF_0000u32.to_le_bytes());
        let p = parse(&rec).unwrap().unwrap();
        assert!(p.names.is_empty());
    }

    #[test]
    fn parses_attribute_lists() {
        let mut v = Vec::new();
        for (t, vcn, rec) in [(ATTR_DATA, 0u64, 0u64), (ATTR_DATA, 0x500, 31)] {
            let mut e = vec![0u8; 0x20];
            e[0..4].copy_from_slice(&t.to_le_bytes());
            e[4..6].copy_from_slice(&0x20u16.to_le_bytes());
            e[8..16].copy_from_slice(&vcn.to_le_bytes());
            e[0x10..0x18].copy_from_slice(&file_ref(rec, 1).to_le_bytes());
            v.extend(e);
        }
        let l = attribute_list(&v);
        assert_eq!(l.len(), 2);
        assert_eq!(l[1].record, 31);
        assert_eq!(l[1].lowest_vcn, 0x500);
    }
}
