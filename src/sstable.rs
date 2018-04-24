use rmps::encode::to_vec;
use rmps::decode::from_slice;

use serde::{Deserialize, Serialize};

use std::cmp::Ordering;
use std::fmt::{Debug, Formatter, Result as FmtResult};
use std::io::{Error as IOError, ErrorKind};
use std::path::PathBuf;

use std::iter::IntoIterator;
use std::borrow::Borrow;

use record_file::buf2string;
use record_file::RecordFile;
use record::Record;

use serde_utils::{serialize_u64_exact, deserialize_u64_exact};

use U32_SIZE;
use U64_SIZE;

const SSTABLE_HEADER: &[u8; 8] = b"DATA\x01\x00\x00\x00";


#[derive(Serialize, Deserialize, Clone)]
struct SSTableInfo {
    record_count: u64,
    group_count: u32,
    indices: Vec<u64>,
    smallest_key: Vec<u8>,
    largest_key: Vec<u8>,
    oldest_ts: u64
}

pub struct SSTable {
    rec_file: RecordFile,
    info: SSTableInfo
}

impl SSTable {
    pub fn open(file_path: &PathBuf) -> Result<SSTable, IOError> {
        if !file_path.exists() {
            return Err(IOError::new(ErrorKind::NotFound, format!("The SSTable {:?} was not found", file_path)));
        }

        let mut rec_file = RecordFile::new(file_path, SSTABLE_HEADER)?;

        let info = from_slice(&rec_file.get_last_record().expect("Error reading SSTableInfo")).expect("Error decoding SSTableInfo");

        let sstable = SSTable { rec_file: rec_file, info: info };

        info!("Opened SSTable: {:?}", sstable);

        Ok(sstable)
    }

    /// Creates a new `SSTable` that is immutable once returned.
    /// * file_path - the path to the SSTable to create
    /// * records - an iterator to records that will be inserted into this `SSTable`
    /// * group_count - the number of records to group together for each recorded index
    /// * count - the number of records to pull from the iterator and put into the `SSTable`
    pub fn new<I, B>(file_path: &PathBuf,  records: &mut I, group_count: u32, count: Option<u64>) -> Result<SSTable, IOError>
        where I: Iterator<Item=B>, B: Borrow<Record>
    {
        assert_ne!(group_count, 0); // need at least 1 in the group
        if count.is_some() { assert_ne!(count.unwrap(), 0); }

        if file_path.exists() {
            return Err(IOError::new(ErrorKind::AlreadyExists, format!("The SSTable {:?} already exists", file_path)));
        }

        // create the RecordFile that holds all the data for the SSTable
        let mut rec_file = RecordFile::new(file_path, SSTABLE_HEADER)?;

        debug!("Created RecordFile: {:?}", rec_file);

        let mut sstable_info = SSTableInfo {
            record_count: 0,
            group_count: group_count,
            indices: vec!(),
            smallest_key: vec!(),
            largest_key: vec!(),
            oldest_ts: 0
        };

        let mut group_indices = vec![0x00 as u64; group_count as usize];
        let mut cur_group_indices_offset = 0;
        let mut cur_key :Vec<u8> = vec![];
        let mut cur_ts = 0;

        // keep fetching from this iterator
        while let Some(r) = records.next() {
            let rec = r.borrow();

            // quick sanity check to ensure we're in sorted order
            if sstable_info.record_count != 0 && rec.get_key() <= cur_key {
                panic!("Got records in un-sorted order: {} <= {}", buf2string(&rec.get_key()), buf2string(&cur_key));
            }

            // take care of our group_indices
            if sstable_info.record_count == 0 {
                // the first time through we just make space for the record_group_indices
                let record_group_indices_buff = serialize_u64_exact(&group_indices);
                cur_group_indices_offset = rec_file.append(&record_group_indices_buff)?;
            } else if sstable_info.record_count % group_count as u64 == 0 {
                // write the current record_group_indices to disk
                let record_group_indices_buff = serialize_u64_exact(&group_indices);
                rec_file.write_at(cur_group_indices_offset, &record_group_indices_buff, true)?;

                // reset the record_group_indices, and write it to the new location
                group_indices = vec![0x00 as u64; group_count as usize];
                let record_group_indices_buff = serialize_u64_exact(&group_indices);
                cur_group_indices_offset = rec_file.append(&record_group_indices_buff)?;
            }

            // append the record to the end of the file, without flushing
            let loc = rec_file.append(&Record::serialize(rec))?;

            // add to our group index
            group_indices[(sstable_info.record_count % group_count as u64) as usize] = loc;

            // add to the top-level indices if needed
            if sstable_info.record_count % group_count as u64 == 0 {
                sstable_info.indices.push(loc);
            }

            // record our current key and ts for use later
            cur_key = rec.get_key();
            cur_ts = rec.get_created();

            // the first time through we set the smallest key, and oldest time
            if sstable_info.record_count == 0 {
                sstable_info.smallest_key = cur_key.to_vec();
                sstable_info.oldest_ts = cur_ts;
            } else if cur_ts > sstable_info.oldest_ts {
                sstable_info.oldest_ts = cur_ts;
            }

            // update our record count
            sstable_info.record_count += 1;

            // break out if we've reached our limit
            if count.is_some() && count.expect("Error unwrapping Some(count)") >= sstable_info.record_count {
                break;
            }
        }

        // write-out our current group_indices
        let record_group_indices_buff = serialize_u64_exact(&group_indices);
        rec_file.write_at(cur_group_indices_offset, &record_group_indices_buff, true)?;

        // update our largest key
        sstable_info.largest_key = cur_key;

        // append our info as the last record, and flush to disk
        let info_buff = to_vec(&sstable_info).expect("Error serializing SSTableInfo");
        rec_file.append_flush(&info_buff)?;

        // create our SSTable
        let sstable = SSTable {
            rec_file: rec_file,
            info: sstable_info
        };

        info!("Created SSTable: {:?}", sstable);

        Ok(sstable)
    }

    pub fn get(&self, key: Vec<u8>) -> Result<Option<Record>, IOError> {
        // check if the key is in the range of this SSTable
        if key < self.info.smallest_key || self.info.largest_key < key {
            return Ok(None);
        }

        // binary search using the indices
        let top_index_res = self.info.indices.binary_search_by(|index| {
            let rec_buff = self.rec_file.read_at(*index).expect("Error reading SSTable");
            let rec :Record = from_slice(&rec_buff).expect("Error deserializing Record");

            rec.get_key().cmp(&key)
        });

        let start_offset = self.info.indices[match top_index_res {
            Ok(i) => i,
            Err(i) => i-1
        }];

        debug!("Top-level binary search: {:?} -> {}", top_index_res, start_offset);

        // need to fetch the group indices array from rec_file
        let group_indices_offset = start_offset - ((self.info.group_count as usize * U64_SIZE) + U32_SIZE) as u64;
        let group_indices_buff = self.rec_file.read_at(group_indices_offset)?;
        let mut group_indices = deserialize_u64_exact(&group_indices_buff);

        // chop the array when we find our first zero offset
        group_indices = group_indices.into_iter().take_while(|i| *i != 0x00 as u64).collect::<Vec<_>>();

        // save the record so we don't need to re-read it
        let mut rec :Record = Record::new(Vec::<u8>::new(), Vec::<u8>::new());

        // binary search through the group indices
        let group_index_res = group_indices.binary_search_by(|index| {
            let rec_buff = self.rec_file.read_at(*index).expect("Error reading SSTable");
            rec = from_slice(&rec_buff).expect("Error deserializing Record");

            rec.get_key().cmp(&key)
        });

        debug!("Group binary search: {:?}", group_index_res);

        // convert from binary_search result to actual result
        let ret = match group_index_res {
            Ok(_) => Some(rec),
            Err(_) => None
        };

        Ok(ret)
    }

    pub fn get_oldest_ts(&self) -> u64 {
        self.info.oldest_ts
    }
}

//impl Drop for SSTable {
//    fn drop(&mut self) {
//        debug!("Calling Drop on SSTable");
//        let info_buff = to_vec(&self.info).expect("Error serializing SSTableInfo");
//
//        self.rec_file.append_flush(&info_buff);
//    }
//}

impl Debug for SSTable {
    fn fmt(&self, formatter: &mut Formatter) -> FmtResult {
        formatter.debug_struct("SSTable")
            .field("record_file", &self.rec_file)
            .field("info", &self.info)
            .finish()
    }
}

impl Debug for SSTableInfo {
    fn fmt(&self, formatter: &mut Formatter) -> FmtResult {
        formatter.debug_struct("SSTableInfo")
            .field("record_count", &self.record_count)
            .field("group_count", &self.group_count)
            .field("smallest_key", &buf2string(&self.smallest_key))
            .field("largest_key", &buf2string(&self.largest_key))
            .field("oldest_ts", &self.oldest_ts)
            .field("indices", &self.indices)
            .finish()
    }
}


impl PartialOrd for SSTable {
    fn partial_cmp(&self, other: &SSTable) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SSTable {
    fn cmp(&self, other: &SSTable) -> Ordering {
        self.info.oldest_ts.cmp(&other.info.oldest_ts)
    }
}

impl PartialEq for SSTable {
    fn eq(&self, other: &SSTable) -> bool {
        self.info.oldest_ts == other.info.oldest_ts
    }
}

impl Eq for SSTable { }


#[cfg(test)]
mod tests {
    use sstable::SSTable;
    use record::Record;
    use std::path::PathBuf;
    use std::thread;
    use std::time;
    use rand::{thread_rng, Rng};
    use std::fs::create_dir;
    use simple_logger;
    use serde_utils::serialize_u64_exact;


    fn gen_dir() -> PathBuf {
        simple_logger::init().unwrap(); // this will panic on error

        let tmp_dir: String = thread_rng().gen_ascii_chars().take(6).collect();
        let ret_dir = PathBuf::from("/tmp").join(format!("kvs_{}", tmp_dir));

        debug!("CREATING TMP DIR: {:?}", ret_dir);

        create_dir(&ret_dir).unwrap();

        return ret_dir;
    }

    fn new_open(num_records: usize, group_size: u32) {
        let db_dir = gen_dir();
        let mut records = vec![];

        for i in 0..num_records {
            let rec = Record::new(serialize_u64_exact(&vec![i as u64]), serialize_u64_exact(&vec![i as u64]));

            records.push(rec);
        }

        {
            SSTable::new(&db_dir.join("test.data"), &mut records.iter(), group_size, None).unwrap();
        }

        let sstable_2 = SSTable::open(&db_dir.join("test.data")).unwrap();
    }

    #[test]
    fn test_new_100_2() {
        new_open(100, 2);
    }

    #[test]
    fn test_new_10000_10() {
        new_open(10000, 10);
    }

    #[test]
    fn test_new_1_10() {
        new_open(1, 10);
    }

    #[test]
    fn test_new_1_1() {
        new_open(1, 1);
    }

    fn get(num_records: usize, group_size: u32) {
        let db_dir = gen_dir();
        let mut records = vec![];

        for i in 0..num_records {
            let rec = Record::new(serialize_u64_exact(&vec![i as u64]), serialize_u64_exact(&vec![i as u64]));

            records.push(rec);
        }

        let sstable = SSTable::new(&db_dir.join("test.data"), &mut records.iter(), group_size, None).unwrap();

        debug!("SSTABLE: {:?}", sstable);

        // look for all the records
        for i in 0..num_records {
            debug!("LOOKING FOR: {}", i);
            let ret = sstable.get(serialize_u64_exact(&vec![i as u64])).unwrap();

            assert!(ret.is_some());
            assert_eq!(ret.unwrap(), Record::new(serialize_u64_exact(&vec![i as u64]), serialize_u64_exact(&vec![i as u64])));
        }
    }

    #[test]
    fn test_get_100_2() {
        get(100, 2);
    }

    #[test]
    fn test_get_10000_10() {
        get(10000, 10);
    }

    #[test]
    fn test_get_1_10() {
        get(1, 10);
    }

    #[test]
    fn test_get_1_1() {
        get(1, 1);
    }

}