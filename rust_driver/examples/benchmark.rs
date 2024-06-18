use open_rdma_driver::tmp_benchmark_entry::benchmark_scheduler;

fn main(){
    benchmark_scheduler(1024*1024*32, 1024*32, 20);
}