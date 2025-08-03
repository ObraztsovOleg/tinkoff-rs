
use crate::t_service_lib::t_service_individual_lib::{
    IndividualServiceClient,
    GetHistoryRequest,
    GetHistoryResponse,
};
use crate::t_service_lib::{
    TService, TSResult
};


#[derive(Debug)]
pub struct IndividualService {
    service: IndividualServiceClient
}

impl IndividualService {
    pub async fn new(service: &TService) -> TSResult<Self> {
        Ok(
            Self { 
                service: service.individual_service().await?
            }
        )
    }

    pub async fn get_archive(&mut self, figi: &str, period_year: i32) -> TSResult<()> {
        let request = GetHistoryRequest {
            figi: figi.to_string(),
            instrument_id: "FIGI".to_string(),
            period_year: period_year,
        };

        let response = self.service.get_archive(request).await?;
        let history = GetHistoryResponse { data: response.bytes().await? };
        if history.data.is_empty() {
            println!("Empty ZIP for {} {}", figi, period_year);
            return Ok(())
        }

        let file_path = format!("history/{}_{}.zip", figi, period_year);
        match std::fs::write(&file_path, &history.data) {
            Ok(_) => println!("ZIP loaded : {}", file_path),
            Err(_e) => println!("Cannot load ZIP: {} {}", figi, period_year)
        }

        Ok(())
    }
}